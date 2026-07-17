# ThetisLink v2.4.3 — Gebruikershandleiding

## Inhoudsopgave

1. [Overzicht](#overzicht)
2. [Server configuratie](#server-configuratie)
3. [Server starten](#server-starten)
4. [Client verbinden](#client-verbinden)
5. [Internet-remote via relay](#internet-remote-via-relay-v240)
6. [Bediening](#bediening)
7. [Apparaten](#apparaten)
8. [Yaesu FT-991A / FTX-1](#yaesu-ft-991a--ftx-1)
9. [Diversity ontvangst](#diversity-ontvangst)
10. [DX Cluster](#dx-cluster)
11. [Macro's](#macros)
12. [Naamconventies](#naamconventies)

---

## Overzicht

ThetisLink is een remote bediening voor de ANAN 7000DLE SDR met Thetis. Het bestaat uit:

- **ThetisLink Server** — draait op de Thetis PC (Windows), bestuurt de radio via TCI
- **ThetisLink Client** — desktop client (Windows/macOS/Linux) met spectrum, waterval en volledige bediening
- **ThetisLink Android** — mobiele client app

De server communiceert met Thetis via TCI WebSocket voor zowel besturing als audio. Audio wordt via Opus codec over UDP verzonden met minimale latency.

### Thetis versie

ThetisLink is getest met en vereist **Thetis v2.10.3.15** (officiële release door ramdor). Dit is de basisversie: alle kernfunctionaliteit (audio, spectrum, PTT, TCI besturing) werkt volledig met ongewijzigde Thetis.

Optioneel is er de **PA3GHM Thetis fork** met ThetisLink-specifieke uitbreidingen. Deze uitbreidingen zitten achter de "ThetisLink extensions" checkbox in Thetis (Setup > Network > IQ Stream). Met de vink uit blijft het stock TCI-extensiegedrag behouden (de fork bevat wel een eigen build-tag, release-notes en About-metadata). ThetisLink detecteert automatisch of extensions beschikbaar zijn en schakelt over. Voordelen van de fork:

- Extended IQ-bandbreedte tot **1536 kHz** (stock cap is 384 kHz), per RX selecteerbaar
- Server-side **CTUN auto-recenter** — DDC volgt automatisch de VFO bij snel tunen
- **Diversity auto-null suite**: Auto/Smart/Ultra met live circle-broadcast voor remote tuning
- **`tci_caps_ex` capability broadcast** — clients detecteren beschikbare extensies automatisch
- Filter-preset push (`rx_filter_preset_ex`), per-RX DDC-rate push (`ddc_sample_rate_ex`)
- Extra TCI `_ex` commando's voor NB2, AGC-auto, VFO-swap, FM-deviation, step/preamp att

### Distributie

ThetisLink wordt gedistribueerd als een zip bestand met de volgende inhoud:

| Bestand | Beschrijving |
|---------|-------------|
| `ThetisLink-Server.exe` | Server executable (Windows) |
| `ThetisLink-Client.exe` | Desktop client executable |
| `ThetisLink-2.4.3.apk` | Android client app |
| `Installatie.pdf` | Installatiehandleiding (Nederlands) |
| `User-Manual.pdf` | Gebruikershandleiding (Nederlands, dit document) |
| `Technische-Referentie.pdf` | Technische referentie (Nederlands) |
| `Installation.pdf` | Installation guide (English) |
| `User-Manual-EN.pdf` | User manual (English) |
| `Technical-Reference.pdf` | Technical reference (English) |
| `LICENSE` | Licentie (GPL-2.0-or-later) |
| `SHA256SUMS.txt` | SHA-256 checksums voor verificatie van de binaries |

> **Configuratiebestanden:** `thetislink-server.conf` en `thetislink-client.conf` zijn niet bijgesloten. Ze worden automatisch aangemaakt met standaardwaarden bij de eerste start van respectievelijk server en client (in dezelfde map als de exe).

### Systeemvereisten

- **Server:** Windows 10/11, Thetis v2.10.3.15 of PA3GHM fork, ANAN 7000DLE (of compatibel)
- **Client:** Windows/macOS/Linux of Android 8+
- **Netwerk:** WiFi of LAN, UDP poort 4580

---

Deze handleiding gaat ervan uit dat ThetisLink is geinstalleerd en geconfigureerd volgens de **Installatiehandleiding** (`Installatie.md`). Daar vind je: installatie van server, desktop client en Android app, Thetis TCI configuratie, firewall-instellingen en netwerk/port forwarding.

---

### Architectuur

```mermaid
flowchart TB
    subgraph "Thetis PC (Windows)"
        Thetis[Thetis SDR Software]
        Server[ThetisLink Server]
        Thetis <-->|"TCI WebSocket :40001<br>audio + IQ + besturing"| Server
    end
    Server <-->|"UDP :4580"| Desktop[Desktop Client<br>egui]
    Server <-->|"UDP :4580"| Android[Android Client<br>Compose]
    Server <--> Amplitec[Amplitec 6/2<br>COM 19200]
    Server <--> Tuner1[JC-4s / JC-3s Tuner #1<br>MCP2221A USB-HID]
    Server <--> Tuner2[JC-4s / JC-3s Tuner #2<br>MCP2221A USB-HID]
    Server <--> SPE[SPE 1.3K-FA<br>COM 115200]
    Server <--> RF2K[RF2K-S PA<br>HTTP :8080]
    Server <--> UB[UltraBeam RCU-06<br>COM]
    Server <--> Rotor[Rotor: EA7HG / PstRotator / G-1000DXC<br>UDP / MCP2221A]
    Server <--> Yaesu[Yaesu FT-991A / FTX-1<br>COM + USB Audio]
```

Alle audio (RX/TX), IQ spectrum data en besturing gaan via één enkele TCI WebSocket verbinding. ThetisLink v2.4.0 gebruikt geen aparte CAT TCP verbinding — TCI dekt alle benodigde commando's, zowel met stock Thetis v2.10.3.15 als met de PA3GHM fork. Geen VB-Cable of andere drivers nodig.

> **Het netwerkpad in beeld:** een geïllustreerde uitleg van hoe audio, spectrum en besturing over het netwerk reizen staat online: **[Het netwerkpad](https://cjenschede.github.io/ThetisLink/Netwerk-uitleg.html)**.

---



## Server configuratie

De basisverbinding met Thetis (TCI/CAT adressen, apparaat COM-poorten) wordt ingesteld tijdens de installatie — zie `Installatie.md`. Hieronder de geavanceerde configuratie-opties.

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

1. Start Thetis en schakel TCI in (Setup > Serial/Network/Midi CAT > Network → TCI Server)
2. Start `ThetisLink-Server.exe`
3. Controleer de verbindingsinstellingen
4. Vink de gewenste apparaten aan
5. Klik **Start**
6. De server luistert op UDP poort 4580

### Server UI

De server toont:
- Verbindingsstatus (TCI WebSocket)
- Actieve apparaat vensters (Tuner, Amplitec, SPE, RF2K, UltraBeam, Rotor, Yaesu)
- Macro knoppen (2 rijen van 12)
- Uptime en client info

---

## Client verbinden

1. Start de client.
2. Bij de eerste run scant de client het lokale netwerk via **mDNS** voor draaiende ThetisLink-servers. Gevonden servers verschijnen automatisch in een dropdown.
3. Selecteer een server uit de lijst, óf voer het IP-adres handmatig in (bijv. `192.168.1.79:4580`).
4. Klik **Connect**.

De client ontvangt automatisch:
- Real-time spectrum en waterval
- VFO frequentie, mode en filter
- S-meter waarden
- Apparaat status (Amplitec, UltraBeam, Yaesu, etc.)
- DX cluster spots

### Auto-discovery werkt niet (mDNS)

Als de mDNS-dropdown leeg blijft maar handmatig IP invoeren wél werkt: mDNS gebruikt UDP multicast (`224.0.0.251:5353`), wat in sommige situaties niet doorkomt.

- **WiFi-routers met AP-isolation / client-isolation**: multicast tussen clients wordt geblokt. Schakel die optie uit in de router-config of gebruik een bedrade verbinding.
- **Intermittent op WiFi** (werkt direct na opstart, faalt na enkele minuten): typisch een laptop-WiFi-driver / power-management probleem dat de multicast-subscription droopt. Workaround: UTP-kabel, of WiFi-driver bijwerken / NIC uit-en-aan.
- **Cross-subnet**: mDNS heeft TTL=1 en springt niet over routers. Server en client moeten in hetzelfde IP-subnet zitten.

Een handmatig ingevuld IP-adres werkt onafhankelijk van mDNS; je hoeft dus niet te wachten op een fix als dit niet meteen werkt.

---

## Internet-remote via relay (v2.4.0)

Op hetzelfde netwerk (LAN/WiFi) verbindt de client rechtstreeks met de server — daar is geen relay voor nodig. Wil je **over het internet** verbinden vanaf een plek buiten je eigen netwerk, dan zijn er twee wegen:

1. **Port-forward** op de router thuis naar de server-PC (UDP 4580). Werkt alleen als je een publiek IP hebt en de router-config kunt aanpassen.
2. **Relay** (nieuw in v2.4.0). Zowel de server (station) als de client verbinden **naar buiten** met een kleine relay-server op een VPS. Geen port-forward nodig, en het werkt ook achter **CGNAT** (waar de provider je geen eigen publiek IP geeft). Dit is de aanbevolen weg voor internet-remote.

De relay draait op een eigen server (VPS) die je zelf host — hij is niet meegeleverd als kant-en-klare download, maar als broncode + Docker-image (zie de installatiehandleiding en `thetislink-relay/DEPLOY-wss.md`). Eén relay bedient één of meer stations; de server-PC hoeft niet bereikbaar te zijn van buitenaf.

De server ondersteunt beide wegen tegelijk: zodra er een relay is geconfigureerd, kiest **elke client zelf** of hij direct of via de relay verbindt. Eén server kan dus meerdere clients tegelijk bedienen waarvan sommige direct en andere via de relay werken — je hoeft niet voor de hele installatie één weg te kiezen.

> **De relay proberen zonder zelf te hosten?** Voor de eerste gebruikers die de relay willen uitproberen kan PA3GHM je — op verzoek en zolang er plek is — tijdelijk toevoegen aan een testrelay. Let op: dit is een **tijdelijke server met een beperkt aantal plekken**, dus zonder garantie op beschikbaarheid of continuïteit. Neem contact op met PA3GHM via [QRZ.com](https://www.qrz.com/db/PA3GHM) (callsign PA3GHM).

### Hoe het verbindt

Zowel het station (server) als de client openen een uitgaande verbinding naar de relay:

- **Besturing + spectrum** lopen over een beveiligde WebSocket (**wss**, TCP-poort 443) — versleuteld door de TLS van de relay (Caddy regelt het certificaat).
- **Audio + PTT** lopen over **UDP** (poort 443) voor minimale vertraging, net als op het LAN.

De relay koppelt de client aan het juiste station op basis van een **room/station-naam** en een **stationsleutel**. Deze gegevens vul je één keer in bij server en client (exacte velden en stappen: zie `Installatie.md`). Zolang beide met dezelfde room en sleutel bij de relay ingelogd zijn, is de verbinding identiek te bedienen als een directe LAN-verbinding.

### Automatische terugval naar TCP (make-before-break)

Sommige netwerken (bedrijfs-WiFi, gast-netwerken, restrictieve mobiele providers) blokkeren UDP. Zonder maatregel zou de audio dan wegvallen terwijl besturing en spectrum blijven werken. ThetisLink lost dit vanaf v2.4.0 automatisch op:

- Merkt de client dat er **geen UDP-audio** meer binnenkomt, dan vraagt hij het station om de audio **door de wss-tunnel (TCP)** te sturen. De overschakeling gebeurt *make-before-break*: het nieuwe pad wordt opgebouwd vóór het oude losgelaten wordt, zodat je geen gat in de audio hoort.
- Zodra UDP weer beschikbaar is, schakelt de verbinding **vanzelf terug** naar UDP (de laagste latency).
- De terugval geldt alleen voor de audio/PTT; besturing en spectrum liepen al over wss.

**Transport-indicator.** Je ziet altijd welk pad de audio nu gebruikt:

- **Desktop:** in het **Server-tabblad**, naast "Audio streams:" — grijs **"Transport: UDP"** in normale toestand, of amber **"TCP fallback"** als de audio tijdelijk door de tunnel loopt.
- **Android:** in het statistieken-paneel, naast "Statistics:" — dezelfde amber **"TCP fallback"**-melding.

Zie je "TCP fallback" tijdens normaal LAN-gebruik zonder relay, dan is er niets aan de hand — de indicator is alleen betekenisvol bij een relay-verbinding. Blijft hij op een relay-verbinding hangen op "TCP fallback", dan blokkeert je netwerk UDP structureel; de audio werkt door, alleen met iets meer vertraging.

> **UDP uitzetten (optioneel).** Weet je op voorhand dat je netwerk UDP blokkeert, dan kun je de client/Android op **alleen-wss** zetten, zodat hij niet eerst UDP probeert. Standaard staat UDP aan (laagste latency) met automatische terugval als vangnet.

### Relay-beheer (dashboard)

Wie de relay host, heeft een **web-dashboard** voor beheer, bereikbaar via de relay (achter TLS, alleen intern/beveiligd toegankelijk — niet vanaf het publieke internet zonder inlog):

- **Inloggen** met een beheerderswachtwoord (veilig opgeslagen met Argon2id-hashing, niet als platte tekst).
- **Apparaten/stations** beheren: het maximum aantal toegelaten apparaten instellen en apparaten blokkeren.
- **Verbruik en quota** per station en apparaat: dataverbruik zien en per station het maximum aantal apparaten/clients en een maandelijkse datalimiet instellen.
- **Database-backup** knop ("Backup DB"): downloadt een consistente kopie van de relay-database (via `VACUUM INTO`, dus zonder de relay te stoppen). Handig voor een periodieke veiligstelling. Gevoelige beheeracties zoals deze export worden met het IP van de aanvrager in het relay-log genoteerd.

De relay-configuratie (stationsleutels, beheerderswachtwoord) staat in een `.env`-bestand op de VPS. Dit bestand bevat geheimen en hoort **nooit** publiek of in een repository terecht te komen — zie `Installatie.md`.

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

Selecteerbaar: LSB, USB, CW, AM, FM, DATA-FM, DIG

### CW keyer (v2.0.0)

In CW-mode is een remote keyer beschikbaar via TCI:

- **CW key down/up:** een toegewezen knop of MIDI-pad activeert `keyer:0,true,duration_ms;` voor een dit/dah, of een PTT-stijl pers-en-loslaat. Server-log toont elke key-event.
- **CW macros:** voorgeprogrammeerde tekst-strings (CQ-call, RST-rapport, eigen call/QTH) worden via `cw_macros:0,text;` naar Thetis gestuurd. Thetis seint ze met de actuele keyer-snelheid (`cw_keyer_speed:wpm;`).
- **Stop:** `cw_macros_stop;` cancelt een lopende macro halverwege.

Snelheid en macro-content zijn instelbaar in de client.

### Filter

De filterbreedte is instelbaar met +/- knoppen. Presets zijn beschikbaar per mode:
- **CW:** 50, 100, 200, 500, 1000 Hz
- **SSB:** 1800, 2400, 2700, 3100, 3600 Hz
- **AM/FM:** 6000, 8000, 10000, 12000 Hz

**Filter preset tracking (v2.0.0, met fork):** met de PA3GHM fork synchroniseert de huidige Thetis-filter-preset (F1, F2, F3, VAR1, VAR2 of NONE) live naar de client. De client toont welke preset Thetis nu actief heeft en je kunt via dezelfde knop-set switchen — geen handmatig dubbel-instellen tussen client en Thetis-UI nodig.

### Volume

- **RX Volume:** ontvangstniveau (TCI `volume` / `rx_volume`)
- **TX Gain:** microfoon voorversterking
- **Drive:** zendvermogen 0-100%
- **Mic AGC:** automatische microfoon gain (aan/uit)

### Noise Reduction & Notch

- **NR:** cyclisch: UIT > NR1 > NR2 > NR3 > NR4
- **ANF:** Auto Notch Filter aan/uit

### PTT (Push-to-Talk)

ThetisLink biedt drie PTT modi:

- **Push-to-talk (spatiebalk):** houd de spatiebalk ingedrukt om te zenden, laat los om te stoppen
- **Toggle:** klik op de PTT-knop om te wisselen tussen zenden en ontvangen
- **MIDI PTT:** aparte MIDI PTT-modus via een toegewezen MIDI controller knop, onafhankelijk van de desktop PTT-modus

**Android — externe BT remote (ZL-01 of vergelijkbaar):** een Bluetooth-knop die zich gedraagt als externe touch-device kan als PTT-knop gebruikt worden. ThetisLink onderschept de touch-events en mappt ze naar PTT down/up. Werkt alleen als het scherm actief is (aanraak-events worden alleen door Android afgeleverd op een wakker scherm).

**PTT-spike-onderdrukking (v2.4.0):** op een tablet/laptop met ingebouwde speaker én microfoon in één behuizing kan de inschakel-plop bij PTT-on meegezonden worden. Zet in de client de optie **"Built-in speaker + mic (PTT spike protection)"** aan: bij PTT wordt de speaker direct gemut en worden de eerste milliseconden mic-audio weggegooid, zodat de plop niet uitgezonden wordt. De **mic gate-delay** is apart instelbaar voor Thetis en Yaesu. Laat de optie **uit** bij een headset of goed geïsoleerde audio (0 ms, geen extra latency). Sinds **v2.4.2** blijft de ontvangst-audio van de andere ontvangers hoorbaar tijdens het zenden; de interne-speaker-mute geldt **alleen** wanneer deze spike-protectie-optie aan staat.

### TX meter (v2.0.0)

Tijdens TX toont de S-meter een TX-meter met:
- **Vermogen** in watts (bijv. `TX  100W`)
- **SWR** kleur-coded — onder 1:2 groen, 1:2-1:3 oranje, boven 1:3 rood (bijv. `SWR 1.50`)

De SWR-waarde wordt door Thetis broadcast via TCI tijdens elke TX-burst en is realtime zichtbaar voor alle verbonden clients.

### TX-modulatiebandbreedte (v2.3.0)

In het **Thetis-tabblad** van de desktop-client stel je de **TX-modulatiebandbreedte** van de hoofdradio in. Twee opties:

- **Volg RX-bandbreedte (Follow RX):** de TX-modulatiefilter loopt 1-op-1 mee met het RX-filter. De handmatige velden worden dan uitgegrijsd. Het scherm toont de meelopende band, bijvoorbeeld `TX volgt RX: 0 .. 2800 Hz`.
- **Onafhankelijk:** zet zelf de onder- en bovengrens (Low/High) van de modulatie.

Het bereik is **0–8 kHz**. De TX-audio loopt op 16 kS/s, dus de modulatie kan tot 8 kHz; staat het RX-filter breder, dan wordt de TX-band op 8 kHz begrensd en meldt het scherm dat (`(RX wider — TX max 8 kHz)`). In de symmetrische modes (AM/SAM/DSB/FM) is het filter rond de draaggolf symmetrisch — sleep je in het spectrum één rand, dan beweegt de andere automatisch mee, net als in Thetis.

> **Tip:** tijdens zenden (PTT actief) worden mode-wijzigingen niet doorgegeven aan Thetis; de mode-knoppen zijn dan uitgegrijsd. Dat voorkomt een desync waarbij Thetis op de oude mode blijft staan terwijl de indicatie de nieuwe toont.

### Spectrum en waterval

- **Zoom:** verstelbaar, geeft nauwkeuriger frequentieweergave
- **Pan:** verschuif het zichtbare spectrum links/rechts (0 = gecentreerd op VFO)
- **Referentieniveau:** verschuif het dB bereik omhoog/omlaag
- **Auto Ref:** automatische referentieniveau-aanpassing op basis van ruisvloer
- **Contrast:** waterval helderheid per band (wordt onthouden)
- **DDC sample rate (v2.0.0):** dropdown voor de IQ-bandbreedte van de DDC. Stock Thetis biedt 384 kHz; met de PA3GHM fork kan per RX gekozen worden uit 48, 96, 192, 384, 768 of 1536 kHz. Hogere rates tonen meer spectrum maar belasten netwerk en CPU zwaarder.

**Server-side CTUN auto-recenter (v2.0.0, met fork):** als de fork-extensie `auto_recenter_ex` actief is, herrekent de server zelf de DDC-center wanneer de VFO snel buiten de huidige DDC-zone gaat. Geen handmatige actie nodig — het spectrum-venster volgt de VFO automatisch.

#### TX spectrum override

Tijdens zenden (TX) wordt het spectrum automatisch aangepast voor goede weergave van het zendsignaal:
- **Referentieniveau:** wordt overschreven naar -30 dB
- **Bereik:** wordt overschreven naar 120 dB
- **Auto Ref:** wordt automatisch uitgeschakeld tijdens TX en de instelling wordt opgeslagen
- Na het loslaten van PTT worden de originele instellingen (inclusief Auto Ref) hersteld met een korte vertraging, zodat het spectrum stabiel terugkeert

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

### Virtuele ontvangers (VRX, v2.2.0)

Naast de twee fysieke ontvangers (RX1/RX2) biedt ThetisLink vanaf v2.2.0 twee **virtuele ontvangers**: **VRX1** (gekoppeld aan RX1/VFO-A) en **VRX2** (gekoppeld aan RX2/VFO-B). Een VRX is een onafhankelijke ontvanger die uit de brede IQ-stroom van de DDC wordt "uitgesneden" — je kunt er dus binnen de huidige DDC-bandbreedte vrij mee rondluisteren zonder de hoofd-VFO te verzetten. Handig om bijvoorbeeld een tweede station op dezelfde band te volgen terwijl je hoofdontvangst op zijn plek blijft.

**Joint VRX pop-out openen:** beide virtuele ontvangers worden samen in één los pop-out venster getoond, met VRX1 en VRX2 naast elkaar. Open het venster vanuit de client; positie en grootte worden onthouden.

**Per-VRX instellingen:**
- **Aan/uit** per VRX — een VRX verbruikt pas netwerk en CPU als je hem inschakelt.
- **Frequentie:** eigen luisterfrequentie, vrij instelbaar binnen de DDC-bandbreedte. Per DDC-positie wordt de laatst gekozen VRX-frequentie onthouden, zodat je bij terugkeren op dezelfde plek verder luistert.
- **Mode:** USB, LSB, AM, SAM of FM.
- **Filter:** eigen filterbreedte per VRX.
- **Volume:** eigen mix-niveau; de VRX-audio wordt samen met RX1/RX2 (en een eventuele Yaesu) in de hoofdaudio gemengd.

**Spectrum, waterval en S-meter:** elke VRX heeft een eigen **hoge-resolutie spectrum + waterval** rond zijn luisterfrequentie en een eigen S-meter. De hoge-resolutie weergave vraag je per VRX aan; de client toont dan een ingezoomd spectrum (met instelbare zoom, referentieniveau, bereik en waterval-contrast).

**Smalle of brede audio:** de VRX-audio is Opus smalband (8 kHz) of breedband (16 kHz). Vanaf v2.3.0 kies je dit **per VRX** (NB/WB/Auto) — zie [Per-VRX audiobandbreedte](#per-vrx-audiobandbreedte-v230) hieronder.

**Persistentie:** de VRX-instellingen (aan/uit, frequentie, mode en filter) worden bewaard en hersteld bij een nieuwe verbinding.

> **Hoe werkt een VRX precies?** Een geïllustreerde uitleg van de hele VRX-signaalketen — van radiogolf tot geluid — staat online: **[Hoe een VRX werkt](https://cjenschede.github.io/ThetisLink/VRX-uitleg.html)**.

### Synchrone AM (SAM) met carrier-PLL (v2.3.0)

In **SAM**-mode gebruikt een VRX vanaf v2.3.0 een echte synchrone AM-demodulator: een **fase-vergrendelde lus (PLL)** haakt aan op de draaggolf van het AM-station en demoduleert ten opzichte van die teruggewonnen draaggolf. Het resultaat is schonere AM dan de oude pseudo-SAM — ook als je een paar Hz naast de draaggolf staat verdwijnt de fluittoon (beat), en de ontvangst blijft stabiel door selectieve fading heen. De lus vangt de draaggolf binnen een bereik van ongeveer ±3 kHz.

**Auto-afstemmen op de draaggolf:** je kunt SAM laten **meelopen met de draaggolf** — de luisterfrequentie (en je VFO) schuift dan automatisch precies op de draaggolf en blijft die volgen, ook als de zender langzaam wegdrijft. Dit is een keuze per VRX; staat hij uit, dan blijft de frequentie staan waar je hem zet en trekt alleen de PLL de fase recht.

### Per-VRX audiobandbreedte (v2.3.0)

Elke VRX heeft vanaf v2.3.0 een **eigen audiobandbreedte-keuze**: **NB** (smalband, 8 kHz), **WB** (breedband, 16 kHz) of **Auto**. Dit staat los van de globale [RX-bandbreedte](#rx-bandbreedte-smalbreed-v220)-schakelaar, zodat je bijvoorbeeld VRX1 smal en VRX2 breed kunt zetten. In **Auto** schakelt de VRX vanzelf naar breedband zodra je het filter breder dan ongeveer 4 kHz opent, en weer terug naar smalband bij een smaller filter.

### RX-bandbreedte (smal/breed, v2.2.0)

Eén schakelaar in de client zet de **RX-audiobandbreedte** voor de Thetis-ontvanger, de VRX-kanalen én de aangesloten Yaesu-radio's tegelijk: smalband (8 kHz) of breedband (16 kHz). Dit geldt alleen voor ontvangst — de zend-audio blijft altijd breedband. De sample-rate van een WAV-opname schaalt automatisch mee met deze instelling.

### WebSDR/KiwiSDR (Desktop)

Ingebouwde WebView voor WebSDR en KiwiSDR ontvangst:
- Frequentie synchronisatie: WebSDR volgt de VFO
- Automatisch muten tijdens zenden
- Favorietenlijst met ster-icoon
- **Herlaad-knop (v2.4.0):** laadt de WebSDR-pagina snel opnieuw na een netwerk-onderbreking, zonder de client te herstarten

### MIDI Controller

Desktop en Android ondersteunen USB MIDI controllers:
- **Scan** knop zoekt beschikbare MIDI apparaten
- **Learn** modus: druk op een MIDI knop/slider, wijs een functie toe
- Beschikbare functies: PTT (met LED), VFO tune, volumes, drive, NR, ANF, mode, band, power
- Encoder stappen: 1 Hz, 10 Hz, 100 Hz, 1 kHz
- **MIDI PTT modus:** aparte PTT-modus voor MIDI, onafhankelijk van de spatiebalk PTT-modus

### Thema en UI-kleuren (v2.4.0)

De desktop-client heeft een **thema-keuze** in het Server-tabblad. Naast de standaardstijl zijn er voorgedefinieerde donkere varianten, plus een volledig instelbaar eigen thema:

- **Classic** — de oorspronkelijke ThetisLink-stijl.
- **Dark** / **Slate** — donkere varianten met minder helderheid, prettiger bij avondgebruik.
- **Custom** — kies zelf de kleuren. Met de kleurkiezers stel je in:
  - **Background** — de achtergrond van de vensters.
  - **Widgets** — de vulkleur van knoppen en velden.
  - **Text** — de tekstkleur.
  - **Slider knop** — de accentkleur van de schuifknoppen en hun rail.

De keuze wordt bewaard in `thetislink-client.conf` en hersteld bij de volgende start. Het thema is puur cosmetisch — spectrum- en waterval-kleuren (het signaalniveau-palet) blijven ongewijzigd zodat signalen op elke achtergrond gelijk afleesbaar blijven.

---

## Apparaten

### Amplitec 6/2 Antenne Schakelaar

Serieel USB verbinding (19200 baud). Toont:
- Huidige schakelstand poort A en B
- 6 antenne posities met configureerbare labels
- Schakel knoppen per poort

### StockCorner JC-4s / JC-3s automatische tuners (multi-tuner via MCP2221A)

Vanaf v2.0.3 ondersteunt de server **twee fysieke tuners parallel**, elk via een eigen Adafruit MCP2221A USB-HID breakout. JC-4s en JC-3s hebben hetzelfde besturingsprotocol — het modellabel is alleen cosmetisch. Per tuner-slot stel je het bord-serienummer en (optioneel) de Amplitec-A-antennepositie in waarachter de tuner fysiek hangt; de server stuurt vervolgens automatisch de juiste tuner aan bij een Tune-actie.

**Hardware-koppeling (per tuner):**
- **GP2** → in serie met een gate-weerstand naar de gate (2N7000) of basis (MMBT3904) van een transistor; bij `HIGH` trekt de transistor de **grijze "start"-draad** van de JC-Control naar GND (mechanisch gelijk aan de start-knop indrukken).
- **GP1** → ADC-ingang op het middenpunt van een **1 MΩ + 1 MΩ 1:1 spanningsdeler** op de **gele "tune-status"-draad**. Idle ≈ 4.5 V, tune-actief ≈ 0 V; de hoge impedantie belast de JC-Control LED-keten niet noemenswaardig.
- **GND** → gemeenschappelijke massa met de JC-Control.
- Het volledige schema (inclusief 2N7000- en MMBT3904-varianten) staat in de technische referentie.

**Eerste keer instellen:**
1. Plug alle MCP2221A-borden in en open het **MCP2221A tuner bridges**-blok onderin het server status-paneel (klap uit met het driehoekje; de stand wordt onthouden tot de volgende keer).
2. Klik **Scan** onder "Detected MCP2221A boards" — alle borden op de USB-bus worden opgelijst met hun pad en huidig serienummer.
3. Voor elk **anoniem** bord (leeg serienummer): vul een unieke naam in onder "Set serial:" (bijvoorbeeld `JC-4s loop` of `JC-3s vertical`) en klik **Program serial**. Het bord onthoudt de naam in EEPROM; klik nogmaals op **Scan** om de nieuwe naam te zien.
4. Voor elk **Tuner1** / **Tuner2** blok in het paneel: kies onder "MCP serial:" het bord dat bij dat slot hoort en kies onder "Amplitec pos:" de antennepositie (1–6) waarachter de tuner fysiek zit. Beide acties triggeren een server-auto-restart zodat de bridge op het gekozen bord opent.

**Per-tuner status-rij toont:**
- Header: tuner-label + "Connected" / "Not connected" / "Error: …"
- **MCP serial** dropdown en **Amplitec pos** dropdown.
- **Live:** actuele spanning op de gele draad (V, na ×2 deler-correctie).
- **Threshold** schuif (0.5–4.5 V, default 2.25 V): de schakelgrens op de gele draad.
- **Hysteresis** schuif (0.1–2.0 V, default 0.50 V): doodband rondom de threshold om transient-ruis te onderdrukken.
- **Edges:** de afgeleide grenzen (`active < … V`, `idle > … V`). Bij een onmogelijke combinatie (bijv. threshold 0.5 V + hysterese 2.0 V → active < 0 V) verschijnt een amber **⚠ clamped**-waarschuwing met hover-tip die uitlegt dat de combinatie nooit zal triggeren — verlaag de hysterese of beweeg de threshold weg van de rand.

**Tune-volgorde (per tuner):**
1. PA standby (SPE/RF2K) als één van beide in Operate staat.
2. GP2 HIGH (start asserted) en wacht tot de gele draad onder de active-edge zakt = tuner ACK.
3. GP2 LOW (start released).
4. Thetis carrier ON (`ZZTU1;`).
5. Wacht tot de gele draad terug boven de idle-edge komt = tune compleet.
6. Thetis carrier OFF (`ZZTU0;`).
7. PA terug naar Operate.

Een timeout treedt op als de tuner binnen 3 s na GP2 HIGH niet ACK't (status **Timeout**), of als de tune-cyclus binnen 30 s niet compleet is (idem). **Abort** breekt de cyclus af en zet GP2 weer LOW. Eén ADC-poll is rate-limited tot 100 ms per bord; de tuner-thread checkt expliciet de sample-timestamp om dubbel-tellen van rate-limited cached samples te voorkomen.

**USB auto-reconnect:** zodra een bridge "Connected" is geweest en de verbinding daarna wegvalt (kabel los, slaap-modus, hub reset, …) probeert de tuner-thread elke 5 s zelfstandig opnieuw te openen. Een succesvolle reconnect reset de timer zodat een volgende drop direct opnieuw geprobeerd wordt — geen server-restart nodig.

> **Tune-knop zichtbaarheid:** De Tune-knop in het hoofdscherm is alleen zichtbaar wanneer er ten minste één Amplitec label naar een woord verwijst dat een tuner herkent (`JC-4s`, `JC4s`, `JC-3s`, `JC3s`, of `Tuner`). De routing naar het juiste fysieke tuner-slot gebeurt automatisch op basis van de actieve Amplitec-A positie — zie [Naamconventies](#naamconventies).

### SPE Expert 1.3K-FA

Serieel USB verbinding. Toont:
- Vermogen, SWR, temperatuur
- Antenne selectie
- Operate/Standby status

### RF2K-S

TCP/IP verbinding (poort 8080). ThetisLink ondersteunt zowel de originele RF2K-S firmware als de aangepaste v190 firmware met uitgebreide drive control.

**Originele firmware — basisfunctionaliteit:**
- Band- en frequentie-uitlezing
- Operate/Standby schakelen
- Tuner bediening (mode, L/C waarden)
- Error status en antenne selectie
- Vermogen, SWR, temperatuur

**Aangepaste firmware (v190+) — extra functionaliteit:**
- Drive vermogen uitlezen en aanpassen (increment/decrement)
- Drive configuratie per band en modulatietype (SSB/AM/Continuous)
- Debug telemetrie (bias spanning, PSU spanning, uptime)
- Controller versie met hardware revisie

ThetisLink detecteert automatisch welke firmware actief is. Met de originele firmware werkt alles behalve drive-bediening.

De RF2K-S kan gereset worden via de server UI wanneer dat nodig is.

### UltraBeam RCU-06

Serieel USB verbinding (19200 baud). Functies:
- **Frequentie display** met band indicatie
- **Direction knoppen:** Normal, 180 graden, Bi-Dir
- **Frequentie stap knoppen:** -100, -50, -25, +25, +50, +100 kHz
- **Sync VFO:** stel de UltraBeam in op de huidige VFO frequentie (A of B, afhankelijk van Amplitec schakelstand)
- **Auto:** automatische frequentie-tracking van de actieve VFO
  - Minimale stap: 25 kHz (voorkomt overbelasting van de motoren)
  - VFO selectie wordt automatisch bepaald via de Amplitec (zie [Naamconventies](#naamconventies))
- **Band presets:** snelkeuze knoppen per band
- **Motor-indicatoren M1 / M2 (v2.0.0):** twee badges naast de voortgangsbalk die per motor aangeven of die op dat moment beweegt. Oranje = motor draait, grijs = motor stilstand. Bij een grote band-wissel (bv. 80m → 10m) zie je vaak even allebei oranje, en wanneer één motor zijn doelpositie eerder bereikt zie je die badge naar grijs gaan terwijl de andere nog door draait.
- **Motor voortgang:** gedeelde progressiebalk tijdens element-verplaatsing. De RCU-06 deelt slechts één voortgangswaarde voor beide motoren samen — exacte per-motor voortgang is niet via de controller beschikbaar.
- **Retract:** trek alle elementen in (met bevestiging)
- **Element weergave:** actuele element lengtes in mm

### Rotor backends

ThetisLink ondersteunt drie rotor-backends. Kies in het server-config-venster onder *Rotor → backend* welke je gebruikt; één tegelijk actief.

In het client-paneel (kompas, GoTo, Stop) is geen verschil zichtbaar — de keuze van backend bepaalt alleen hoe de server met de rotor-hardware praat.

#### EA7HG Visual Rotor

Directe UDP-verbinding met de EA7HG Visual Rotor software (Prosistel-protocol). Vul het adres van de Visual Rotor in (bv. `192.168.1.60:3010`); verder geen configuratie nodig.

#### Yaesu G-1000DXC via Adafruit MCP2221A (v2.1.0+)

Directe aansturing van de Yaesu G-1000DXC EXT CONTROL connector vanaf de ThetisLink-server PC via een Adafruit MCP2221A breakout (op 5 V gejumperd). Geen extra controller-PCB of derde-partij software nodig. Vervangt EA7HG voor wie liever ThetisLink in-process houdt.

**Hardware**

- Adafruit MCP2221A breakout (#4471) met de 3 V solder-jumper aan de onderkant doorgesneden en de 5 V pad gebrugd
- 2× BST82 (SOT-23) als low-side switches op pin 1 (R/CW) en pin 2 (L/CCW) van de Yaesu DIN-7
- 2× 100 kΩ gate-pulldown (voorkomt spontane rotatie tijdens USB-reset)
- 1× 1,8 kΩ + 1× 2,2 kΩ spanningsdeler op pin 4 (position-feedback) naar GP3 ADC; **niet** 1,8 kΩ + 10 kΩ — die clipt boven ~365° op rotors waar pin 4 boven 4,8 V uitkomt
- Optioneel: 10 µF condensator parallel aan de 2,2 kΩ tegen 100 Hz netvoedings-ripple
- 7-pin mini-DIN kabel naar de rotor

**Setup in ThetisLink-server**

1. Open de **MCP2221A** sectie in het Status-paneel.
2. Klik **Scan** — de Adafruit verschijnt als "Unprogrammed" board.
3. Kies *function = Rotor*, vul een naam in (bv. `rotor1`), klik **Add**. Het bord krijgt de USB-serial `rot_<naam>` geschreven naar zijn EEPROM.
4. Herstart de server zodat het bord opgepakt wordt.
5. Stel onder *Rotor → backend* in op **Yaesu G-1000DXC (MCP2221A)**.

**Kalibratie**

Voordat GoTo werkt moet de spanning-naar-graden mapping ingelezen worden:

1. Draai handmatig naar het CCW-eindpunt (mechanisch hard tegen de stop, 0°).
2. Klik **Park CCW (0°)** in de rotor-row.
3. Draai handmatig naar het CW-eindpunt (450° bij de G-1000DXC).
4. Klik **Park CW (450°)**.

De server slaat de twee spanningen op als `v_at_0deg` en `v_at_max_deg` in `thetislink-server.conf`. Bij hardware-wijziging (deler-verhouding) altijd opnieuw kalibreren.

**Configuratie per rotor**

- *max°* — fullscale van de rotor (default 450 voor G-1000DXC)
- *ramp* — soft-start / soft-stop snelheid in %/sec (1-200, default 50). Lager = traagheidsvriendelijker voor zware antennes; hoger = sneller reactief.
- *shortest route* — alleen zichtbaar als `max_deg > 360`. Bij aanvinken kiest een GoTo de kortste mechanische route via de overlap-zone (bv. huidig 350°, target 30° → CW via 390° i.p.v. CCW via 0°). Default uit zodat "ga naar 30°" letterlijk op 30° fysiek eindigt.

**Diagnostiek**

De rotor-row toont de live positie in hele graden, de mediaan pin-4 spanning, de laatste raw ADC sample en de peak-to-peak spread (ruis-indicator). Tijdens een GoTo zie je het soft-start ramp-omhoog en de soft-stop ramp-omlaag in de DAC-slider. Bij stilstand poll't de server op 1 Hz (60-sample mediaan); bij beweging op 30 Hz (10-sample mediaan).

**Test-knoppen + speed-slider**

De CW/CCW/Stop knoppen + DAC speed-slider in de rotor-row staan los van de GoTo-loop. Zodra je de slider beweegt of een test-knop indrukt schakelt de server naar *manual mode* en respecteert je instelling tot een client een nieuwe GoTo/Stop/CW/CCW commando stuurt.

#### PstRotator (XML/UDP)

PstRotator (yo3dmu) ondersteunt vrijwel alle merken rotor-hardware. Aanbevolen wanneer je geen EA7HG Visual Rotor gebruikt of een rotor wilt aansturen die niet rechtstreeks door ThetisLink wordt ondersteund.

**Vereisten**

- PstRotator (of variant zoals PstRotatorAZ voor azimuth-only) draait op een PC in hetzelfde LAN. Dat mag dezelfde PC zijn als de ThetisLink-server of een andere.

**Setup in ThetisLink-server** (Connecties → Rotor)

1. *Backend* = **PstRotator (XML/UDP)**.
2. *PstRotator host* = IP-adres van de PC waar PstRotator op draait.
3. *Poort* = `12000` (PstRotator default).
4. *Feedback poort (lokaal)* = `12001` (= PstRotator's listener-poort + 1).
5. *Heeft elevation* = alleen aan bij een AZ+ELE rotor (PstRotator); uit bij PstRotatorAZ.

**Setup in PstRotator**

1. *Communication → UDP Control Port…* → `12000`.
2. *Setup → UDP Control* aanvinken.
3. In de UDP-instellingen het **IP van de ThetisLink-server-PC** invullen. PstRotator stuurt z'n positie-feedback naar dit IP op poort 12001.

**Firewall**

- ThetisLink-server-PC: inbound UDP 12001 toestaan voor `ThetisLink-Server.exe`. Een app-toelating via Microsoft Defender Firewall (`Allow an app through firewall`) dekt dit automatisch.
- PstRotator-PC: inbound UDP 12000 toestaan voor `PstRotator.exe` of `PstRotatorAZ.exe`. PstRotator vraagt hier vaak zelf om bij eerste start.

**Bekende beperking — geen doel-lijn voor PstRotator-gestuurde GoTos.** Wanneer je een doel in PstRotator's eigen compass-cirkel klikt, ziet TL2 alleen de actuele positie-feedback (`AZ:nnn.n`) en niet het target. De doel-lijn in TL2's rotor-window wordt daarom niet getekend voor zo'n GoTo — alleen de naald loopt mee. Dit is een limiet van PstRotator's outgoing feedback-protocol, geen TL2-bug.

### Externe input: PstRotator of Log4OM rechtstreeks op de Adafruit-rotor (v2.1.1+)

Vanaf v2.1.1 luistert de server parallel aan de actieve rotor-backend op UDP+TCP poort `12001` (configureerbaar via `pstrotator_listen_enabled` / `pstrotator_listen_port` in `thetislink-server.conf`). Dat maakt het mogelijk om Log4OM of een externe PstRotator-instantie **direct de Adafruit-rotor** te laten besturen, zonder dat je de rotor-backend hoeft te switchen. De listener accepteert vier protocol-formaten (auto-detect per packet):

| Protocol | Goto-commando | Query | Bron |
|---|---|---|---|
| Yaesu GS-232A/B | `M<nnn>\r` | `C\r` → `+<nnn>\r` | Vrijwel alle ham-software |
| Prosistel binair (EA7HG) | `\x02AG<nnn>\r` of `AAG<nnn>\r` | `\x02A?\r` → `\x02A,?,<nnn>,<R\|B>\r` | PstRotator EA7HG-mode |
| PstRotator native XML | `<PST><AZIMUTH>nnn</AZIMUTH></PST>` | `<PST>AZ?</PST>` → `AZ:<nnn.n>\r` | PstRotator native, Log4OM |
| AZ-tekst | `AZ:nnn.n\r` | — | PstRotator UDP-output |

De TCP-pad is bidirectioneel: als een TL2-client (desktop, Android, of server-UI) zelf een nieuw target zet, pusht de listener `M<nnn>\r` of `\x02AG<nnn>\r` (afhankelijk van het gedetecteerde protocol) terug naar PstRotator. **Let op:** PstRotator's client-mode UI laat zo'n extern gestuurde target meestal niet visueel zien — dat is een beperking van het GS-232A/Prosistel protocol-ontwerp, niet van TL2.

#### Setup PstRotator → Adafruit rotor (zonder PstRotator als backend)

Wanneer je rotor-backend op **Yaesu G-1000DXC (Adafruit MCP2221A)** staat:

1. In TL2 server-UI: PstRotator listener aanvinken (default poort `12001`).
2. In PstRotator: kies een controller-type. **Aanbevolen: TCP-client** (Setup → "Start as TCP client") met host = TL2-IP en poort `12001`. Alternatief is een controller met UDP-output (EA7HG, GS-232A) op dezelfde poort.
3. PstRotator commandeert de Adafruit-rotor direct via de listener. Server-log toont `compass X° → mech Y°` voor elke klik.

#### Setup Log4OM → Adafruit rotor (PstRotator helemaal niet meer nodig)

Log4OM ondersteunt alleen PstRotator als rotor-protocol. Truc: laat Log4OM denken dat TL2 **PstRotator is**.

1. **PstRotator afsluiten** op de Win4OM-PC (volledig stoppen).
2. In **Log4OM → Settings → External Services → PstRotator** (of equivalent paneel):
   - **Host** = TL2-server IP (bijv. `192.168.1.97`) — **wijzig van `localhost` / `127.0.0.1`**
   - **Port** = `12001` (TL2's PstRotator listener-poort)
3. Klaar — klik in Log4OM op een DX-spot, de Adafruit-rotor draait direct naar de berekende richting.

Log4OM stuurt voor elke spot een handvol PstRotator-XML packets (azimuth + callsign + naam + QTH + frequentie + mode + grid + comment + continent). TL2 verwerkt alleen de azimuth en negeert de metadata-tags stil. Geen tussenliggend programma nodig, geen UDP-simulator-drift, één configuratie-stap.

#### Beperkingen en bekende gedragsregels

- **Aan/uitzetten vereist server-restart.** De listener-threads worden alleen gespawned bij server-start. Toggle `pstrotator_listen_enabled` in de UI of het conf-bestand werkt pas na een herstart van de server. Stoppen is wel direct (server-stop sluit de poort binnen ~500 ms).
- **Manual rotate (`R\r` / `L\r` in GS-232A) wordt genegeerd.** De listener accepteert alleen target-gestuurde commando's. Continu draaien zonder eindpunt is een hardware-knop functie en moet via de rotor-UI van TL2 zelf gebeuren.
- **Stop-commando's wél doorgezet** (`S\r` in GS-232A, `\x02AR\r` / `AAR\r` / `AG999` in Prosistel, `<STOP>` in PstRotator-XML) — die stoppen de rotor onmiddellijk via de actieve backend.

#### Diagnose

De RX-packet-log staat default op debug-level zodat de server-log niet vervuilt met de 2 Hz status-queries. Voor diagnose: start de server met `RUST_LOG=debug` voor de volledige RX-stream. Goto-events, connect/disconnect en parse-warnings blijven altijd op info-level zichtbaar.

---

## Yaesu FT-991A / FTX-1

ThetisLink kan een Yaesu FT-991A transceiver aansturen als tweede radio naast de ANAN. De Yaesu wordt verbonden via een serieel USB COM-poort.

### Functies

- **Frequentie:** uitlezen en instellen van de huidige frequentie
- **Mode:** uitlezen en instellen (LSB, USB, CW, AM, FM, DATA-FM, DIG)
- **VFO A/B:** schakelen tussen VFO A en VFO B
- **Geheugenkanalen:** worden automatisch ingeladen bij het inschakelen van de Yaesu in de server. Kanalen met naam worden weergegeven in de UI. Edit + "Write radio" past frequentie, naam, mode, shift en tone-mode (aan/uit) per kanaal aan. **Let op (v2.1.1+):** de specifieke CTCSS-tone-frequentie wordt niet door TL2 geschreven — alleen "tone aan/uit" gaat mee. Tone-frequentie per kanaal stel je in op de FT-991A zelf.
- **Menu editor:** Yaesu menu-instellingen uitlezen en wijzigen via de server UI
- **Audio:** de Yaesu USB audio wordt door de server gecaptured en via het AudioRx2 kanaal naar de client gestuurd, waar het gemixt wordt met het ANAN RX-signaal
- **Auto-DFM tijdens TX (v2.0.0):** zie subsectie hieronder

### Auto-DFM PTT-toggle (FM ↔ DATA-FM)

Op de FT-991A werkt USB-mic-TX in stand FM niet — alleen DATA-FM accepteert USB-mic-audio. Voor remote-FM-werken zou je daarom de Yaesu permanent in DATA-FM moeten zetten, maar dan luister je ook in DATA-FM (geen squelch, andere filtering).

Vanaf v2.0.0 schakelt ThetisLink hier automatisch tussen:

- **PTT-press** in stand FM ('4'): server stuurt `MD0A;` (DATA-FM) → korte settle → `TX1;`. Yaesu zendt nu via USB-mic-audio.
- **PTT-release** na auto-DFM-cyclus: server stuurt `TX0;` → settle → `MD04;` (terug naar FM). RX-audio is weer normale FM.
- **Memory-mode:** als Yaesu in Memory-mode op een FM-kanaal staat, bewaart de server het kanaal-nummer bij PTT-on en herstelt het kanaal via `MC<nnn>;` na PTT-off, zodat je in Memory-mode blijft op het oorspronkelijke kanaal.

Auto-DFM is niet actief in DATA-FM ('A'), USB ('2'), FM-N ('B') of andere modes — die houden hun normale TX-pad.

Bekende beperkingen: mode-wijziging tijdens active TX kan de auto-restore verwarren; vermijd mode-knoppen drukken terwijl PTT actief is. Bij server-crash tijdens TX moet je handmatig terug naar FM (de server kan z'n tussenstaat niet automatisch herstellen).

### SSB-zenden via USB-audio (v2.4.0)

Vanaf v2.4.0 kun je de Yaesu ook in **SSB (LSB/USB)** remote laten zenden met de USB-microfoonaudio — voorheen accepteerde de radio USB-mic-TX alleen in (DATA-)FM. De server kiest bij het zenden automatisch de juiste modulatiebron:

- **FT-991A:** bij PTT in SSB (LSB/USB) schakelt ThetisLink **SSB MIC SELECT = REAR** en **SSB PORT SELECT = USB** (EX106/EX109), zodat de USB-audio de zender moduleert; bij loslaten wordt de oorspronkelijke routing hersteld. (Voor AM geldt hetzelfde principe met de AM-menu's.)
- **FTX-1:** ThetisLink laat de **interne automatische modulatiebron** van de FTX-1 ongemoeid — die kiest de USB-audio zelf, dus er wordt géén menu-instelling geforceerd.
- **Let op:** de automatische DATA-omschakeling geldt alleen voor **FM → DATA-FM**, niet voor SSB. SSB blijft in de gewone SSB-mode en gebruikt de REAR/USB-routing hierboven.

De SSB-USB-routing wordt standaard **per PTT** toegepast en tussen overs teruggezet (in opt-out-modus presence-based hersteld). Met de **Exit**-knop zet je de radio terug naar zijn vaste MIC/DATA-basis. De TX-audio-uitgang wordt herhaald geopend tot het USB-CODEC-device vrij is.

### TX-audiobewerking: compressor + AGC (v2.4.0)

Voor de Yaesu-zendtak biedt de client (desktop én Android) een **spraakcompressor** en een **AGC-schakelaar**, naast de bestaande **TX-EQ** — allemaal **per radio** instelbaar. Zo geef je de USB-modulatie meer draagkracht zonder de Yaesu-instellingen zelf aan te passen. De AGC-cyclus loopt netjes FAST → MID → SLOW → AUTO.

### Clarifier (RIT/XIT) (v2.4.0)

Beide Yaesu-radio's hebben een **clarifier**: schakel RIT en/of XIT in, verstel de offset in stappen en wis 'm met één knop. Handig om zender en ontvanger los van elkaar iets te verschuiven zonder de VFO te verzetten.

### Yaesu-bediening op Android (v2.4.0)

De Android-client heeft nu vrijwel dezelfde Yaesu-bediening als de desktop: een **radio 1 / radio 2-selector** (dual-radio), een volledig inklapbaar **DSP-paneel** (ATT/AGC/NB/NR/IPO/Contour/APF/Notch/Proc/AMC), **touch-frequentietuning** (grote tikbare digit-tuner + stapper), de interne **ATU** (Tune + ATU aan/uit) en de **clarifier**.

### Databesparing op mobiel (v2.4.0)

Om mobiel dataverbruik te beperken stuurt de server geen Thetis-RX-audio meer naar clients die alleen een Yaesu beluisteren, en wordt Yaesu-data alleen gestreamd zolang het Yaesu-venster open/actief is (met een korte spectrum-grace bij hervatten). Zo betaal je op 4G/5G niet voor streams die je niet gebruikt.

### Configuratie

```
yaesu_port=COM5
yaesu_enabled=true
```

De Yaesu audio wordt automatisch afgespeeld op de client als het apparaat is ingeschakeld.

### Tweede radio (FT-991A + FTX-1, dual-radio, v2.2.0)

Vanaf v2.2.0 kan een **tweede Yaesu-radio** naast de eerste draaien als een **onafhankelijk kanaal** (slot 1). Beide radio's hebben hun eigen CAT-COM-poort, eigen USB-audio en hun eigen frequentie, mode, PTT en geheugenkanalen. Je kunt twee FT-991A's, twee FTX-1's of een mix gebruiken.

**Automatische modeldetectie:** bij het opstarten leest de server het radio-model uit via het CAT-commando `ID;`. De respons bepaalt het model:

| ID-respons | Model |
|---|---|
| `0670` | FT-991A |
| `0840` | FTX-1 |

Als het gedetecteerde model niet overeenkomt met het ingestelde slot, logt de server een waarschuwing (mogelijk zijn de twee USB-radio's verwisseld bij het enumereren). De server stuurt daarnaast een `RadioInfo`-bericht naar de clients, zodat dual-radio-bewuste clients de panelen met het juiste model labelen.

### FTX-1 software-squelch (v2.2.0)

De **hardware-squelch van de FTX-1 dempt zijn USB-audio niet** — op een FM-kanaal stroomt er dus continu ruis mee. ThetisLink heeft daarom een **server-side software-squelch** die de busy-status van de radio uitleest (CAT `RI`-respons) en de audio naar stilte laat uitfaden zodra de squelch dicht is.

- Werkt **alleen in de FM-familie** (FM, FM-N, DATA-FM). In SSB, CW, AM en data-modes heeft de busy-vlag geen zinvolle betekenis, dus daar wordt de audio altijd doorgelaten.
- De squelch-knop op de radio zelf is de drempel; de server volgt simpelweg of de radio "busy" meldt.
- De gate faadt zacht (met een korte hang-tijd) zodat snelle openen/sluiten geen geflutter geeft.

### FTX-1 WIRES-X (EX-menu)

De **WIRES-X EX-menu-velden** van de FTX-1 zijn toegevoegd aan de menu-editor, zodat je de WIRES-X-instellingen van de radio via de server-UI kunt uitlezen en wijzigen.

### Twee identieke "USB Audio CODEC"-apparaten onderscheiden (`#N`)

Twee Yaesu-radio's presenteren zich in Windows allebei als een apparaat met dezelfde naam ("USB Audio CODEC"). Om ze in de audio-apparaatkeuze uit elkaar te houden gebruik je een **`#N`-indexsuffix** achter de naam:

```
USB Audio CODEC      → het eerste apparaat met die naam
USB Audio CODEC#1    → idem (eerste match)
USB Audio CODEC#2    → het tweede apparaat met die naam
```

De server kiest het **N-de apparaat** dat op de naam matcht (`#2` = de tweede). Zonder suffix wordt het eerste matchende apparaat gebruikt. Het server-log toont per radio de gekozen apparaatnaam, zodat je kunt controleren welk CODEC aan welke radio hangt.

---

## Diversity ontvangst

ThetisLink ondersteunt diversity ontvangst via RX1 en RX2. Dit combineert twee antennes (bijvoorbeeld de ANAN op twee verschillende antenne-ingangen) voor verbeterde ontvangst.

### Gebruik

1. Schakel RX2 in via de client
2. Stel beide VFO's in op dezelfde frequentie (of gebruik VFO Sync)
3. De server stuurt onafhankelijke spectrum- en audiostreams voor RX1 en RX2
4. Gebruik de volume-regelaars om de balans tussen RX1 en RX2 in te stellen

Diversity werkt ook in combinatie met de popout vensters (Joined view) voor een overzichtelijke weergave van beide ontvangers.

### Smart en Ultra Auto-Null (Diversity)

Naast handmatige diversity-instelling biedt ThetisLink twee automatische null-algoritmen:

- **Smart:** voert een AVG sweep uit over 360° + 90° in stappen van 5° met settle-tijd per stap. Duurt circa 9 seconden. Betrouwbaar en nauwkeurig.
- **Ultra:** continue forward/backward sweep zonder settle-tijd, aanzienlijk sneller (circa 5 seconden). Geschikt als je snel een nulpunt wilt vinden.

Beide algoritmen zijn beschikbaar in de dropdown naast de **Auto Null** knop. Na afloop wordt het resultaat getoond in dB verbetering: groen betekent een goed nulpunt, oranje betekent weinig verschil met de uitgangssituatie.

**Live circle-broadcast (v2.0.0, met fork):** tijdens een Smart of Ultra sweep zendt de PA3GHM fork de actuele phase/gain-positie realtime mee. De client toont de huidige meting als bewegende stip op de circle-plot, zodat je live ziet hoe het algoritme door het zoekgebied gaat. Dit werkt ook als de sweep door een andere client gestart is — alle verbonden clients zien dezelfde live-trace.

Op Android is er een **Smart Null** knop die het resultaat in dB toont na afloop.

---

### Audio opname en afspelen

De client heeft een ingebouwde audio recorder en speler:

- **Record** knop in de Server tab met per-kanaal checkboxes: **RX1**, **RX2**, **Yaesu 1**, **Yaesu 2**, **VRX1** en **VRX2** (v2.4.0) — elk vakje verschijnt alleen als dat kanaal beschikbaar/ingeschakeld is. Selecteer welke kanalen je wilt opnemen
- Opnames worden opgeslagen als WAV bestanden (mono) naast de client executable, met een timestamp in de bestandsnaam. De sample-rate volgt de [RX-bandbreedte](#rx-bandbreedte-smalbreed-v220)-instelling — 8 kHz (smal) of 16 kHz (breed)
- **Play** knop speelt de laatste opname af:
  - **Zonder PTT:** het opgenomen geluid wordt via de speakers afgespeeld, gemixt met de ontvangst-audio
  - **Met PTT ingedrukt:** de opname vervangt de microfoon (TX inject) — handig om je eigen modulatie te testen of een CQ-bericht te herhalen
- **Stop** knop breekt het afspelen af. Aan het einde van de opname stopt het automatisch.
- **Play volume**-schuif (0–2×) naast Play/Stop regelt het niveau van een opname die naar de zender gaat (v2.4.2).
- Bij TX-inject gaat de opname **schoon op lijnniveau** naar buiten: de microfoon-verwerkingsketen (5-bands EQ, compressor, AGC en de mic-gain-boost) wordt omzeild, zodat een opname die al op lijnniveau staat niet meer overgemoduleerd wordt. Voor de hoofdradio wordt de zend-**TX-EQ van Thetis tijdelijk omzeild en daarna exact op de vorige stand hersteld**; bij een Yaesu wordt de mic-EQ op dezelfde manier overgeslagen (v2.4.2).
- Terwijl een opname wordt uitgezonden toont de audio-niveaubalk het **uitgezonden niveau**, niet de (gemute) microfoon (v2.4.2).

---

### Spectrum en waterval kleuren

Het spectrum en de waterval gebruiken een signaalniveau-afhankelijke kleurschaal:

- **Blauw** (zwak signaal) → **cyaan** → **geel** → **rood** → **wit** (sterk signaal)
- Zowel de spectrumlijn als de waterval gebruiken dezelfde kleurschaal
- De kleuren zijn identiek op desktop en Android

---

### Remote beheer

In de Server tab zit een **Remote Reboot / Shutdown** knop waarmee je de server-PC op afstand kunt herstarten of afsluiten:

- Na het klikken kies je tussen **herstart** of **afsluiten**
- Voor reboot is een `ThetisLinkReboot` scheduled task vereist op de server-PC (zie Installatie.md voor de configuratie)

---

### Audio modus (Mono/BIN/Split)

In de RX1 sectie zit een dropdown voor de audio-modus:

- **Mono:** RX1 en RX2 audio worden gemixt op beide oren (standaard)
- **BIN:** RX1 binaural audio op links en rechts + RX2 (vereist dat Thetis in BIN-modus staat)
- **Split:** RX1 op het linkeroor, RX2 op het rechteroor, met onafhankelijke volume-regelaars per kanaal

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

**Click-to-tune (v2.0.0):** klik op een spot-label op het spectrum (15-pixel snap-zone) om VFO direct naar de spot-frequentie te tunen. De snap-zone houdt rekening met label-overlap: clicks dichter bij een ander label gaan naar dat label. Als je buiten de snap-zone klikt valt het terug op normale click-to-tune (afgerond op 1 kHz).

---

## Macro's

De server ondersteunt 24 programmeerbare macro knoppen in 2 rijen:
- **Rij 1:** F1 t/m F12 (typisch VFO A presets)
- **Rij 2:** ^F1 t/m ^F12 (typisch VFO B presets)

### Macro acties

Elke macro kan een reeks acties bevatten:
- **CAT commando:** bijv. `ZZFA00014292000;` (stel VFO A in op 14.292 MHz)
- **Delay:** bijv. `delay:200` (wacht 200ms)
- **Tune:** start de tuner die bij de actieve Amplitec-A positie hoort (één of twee fysieke tuners; zie [StockCorner JC-4s / JC-3s automatische tuners](#stockcorner-jc-4s--jc-3s-automatische-tuners-multi-tuner-via-mcp2221a))

### Macro configuratie

Macro's worden opgeslagen in `thetislink-macros.conf`:
```
macro_0_label=20m 14292
macro_0=ZZFA00014292000; ZZMD01;
```

### Veelgebruikte CAT commando's

| Commando | Beschrijving |
|---|---|
| `ZZFA00014292000;` | VFO A naar 14.292 MHz |
| `ZZFB00007073000;` | VFO B naar 7.073 MHz |
| `ZZMD00;` | VFO A mode naar CW |
| `ZZMD01;` | VFO A mode naar LSB |
| `ZZME00;` | VFO B mode naar CW |
| `ZZME01;` | VFO B mode naar LSB |

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
  - Als Amplitec poort **B** op de UltraBeam positie staat -> volgt **VFO B**
  - Als Amplitec poort **A** op de UltraBeam positie staat -> volgt **VFO A**
  - Geen match -> default **VFO A**

### JC-4s / JC-3s tuner integratie (multi-tuner)

De Amplitec label voor elke tuner-uitgang moet één van deze woorden bevatten (hoofdletter-ongevoelig):
- `JC-4s`
- `JC4s`
- `JC-3s`
- `JC3s`
- `Tuner`

**Wat dit oplevert:**
- De **Tune** knop in het hoofdscherm is alleen zichtbaar als ten minste één Amplitec label een van deze woorden bevat.
- Wanneer de Amplitec-A naar een positie wordt geschakeld die in het server status-paneel aan een fysiek tuner-slot gekoppeld is (zie [tuner-blok](#stockcorner-jc-4s--jc-3s-automatische-tuners-multi-tuner-via-mcp2221a)), routeert de server een Tune-actie automatisch naar de juiste fysieke tuner — de andere tuner blijft idle.

**Voorbeeld configuratie (twee tuners):**
```
amplitec_label1=JC-4s loop
amplitec_label2=JC-3s vertical
amplitec_label3=Dipole
amplitec_label4=Beverage
amplitec_label5=DummyLoad
amplitec_label6=UltraBeam
```

In dit voorbeeld:
- Positie 1 = JC-4s loop → in het server status-paneel toegewezen aan **Tuner1**, MCP serial `JC-4s loop`.
- Positie 2 = JC-3s vertical → toegewezen aan **Tuner2**, MCP serial `JC-3s vertical`.
- Positie 6 = UltraBeam → Sync VFO / Auto tracking voor de UltraBeam (zie [UltraBeam integratie](#ultrabeam-integratie)).

Een Tune-druk bij Amplitec-A op positie 1 start fysiek Tuner1; positie 2 start Tuner2. Alleen één van beide draait tegelijk — de PA-orchestration en RF-carrier worden door de actieve tuner gecoördineerd.

---

## Probleemoplossing

Voor verbindings- en installatieproblemen (server start niet, client kan niet verbinden, firewall, COM-poorten, wachtwoord en 2FA), zie `Installatie.md`.

### Audio hakkelt

Hoge loss% (zichtbaar onderaan de client) duidt op een netwerkprobleem. Probeer een bedrade verbinding in plaats van WiFi. Op mobiel (4G/5G) past de jitter buffer zich automatisch aan, maar bij hoge packet loss blijft audio haperen.

### BT headset niet herkend (Android)

Koppel de headset opnieuw via Android Bluetooth-instellingen en herstart de ThetisLink app.

**EQ profiel auto-switch (v2.0.0):** ThetisLink Android houdt twee aparte TX-EQ profielen bij — één voor de interne mic (`mic_profile_android_mic`) en één voor de BT headset (`mic_profile_android_bt`). Bij PTT-on detecteert de app of er een actieve BT-headset is en kiest automatisch het bijbehorende profiel. Configureer beide profielen via Setup → TX EQ; bij twijfel welk profiel actief is, kijk in de PTT-status van de app.

### UltraBeam timeout bij snel stappen

De UltraBeam RCU-06 heeft een beperkte serieel commando snelheid. Bij snel achter elkaar drukken op stap-knoppen worden tussenliggende commando's overgeslagen en alleen het laatste verzonden. Dit is normaal gedrag en voorkomt overbelasting.

### Spectrum en waterval lopen niet synchroon

Als het spectrum (lijn) en de waterval niet synchroon lopen bij het pannen, herstart de client en controleer dat server en client beide op de actuele versie draaien.

---

## Versiegeschiedenis

| Versie | Hoogtepunten |
|---|---|
| **2.4.3** | **Duidelijkere relay-verbinding + kleur-gecodeerde clientlijst + slider-muiswiel.** Geen wire-protocol-wijziging (`VERSION` blijft 3, volledig interoperabel met v2.4.x); stock Thetis v2.10.3.15 volstaat; geen fork-wijziging; desktop + Android beide bijgewerkt (APK herbouwd). Bij een relay-verbinding toont het verbindingsgedeelte **"Via relay: &lt;station&gt;"** + relay-status i.p.v. het (irrelevante) directe server-IP. De clientlijst op de server **kleurt** elke client naar verbindingstype (direct = blauw, relay = cyaan). **Muiswiel-scroll op elke desktop-slider.** Een **herstart-melding** verschijnt nu bij het aan- én uitzetten van de relay (desktop + Android). Docs verduidelijken dat de server **beide** methodes tegelijk bedient — elke client kiest zelf. |
| **2.4.2** | **Bugfix-patch (opgenomen audio afspelen via de radio).** Eén additieve wire-protocol-control (`ThetisTxeq = 0x90`); `VERSION` blijft 3, dus een directe verbinding blijft interoperabel met v2.4.0/v2.4.1. Stock Thetis v2.10.3.15 volstaat; geen fork-wijziging; Android functioneel ongewijzigd (APK herbouwd). **Opgenomen audio uitgezonden via de radio is niet meer overgemoduleerd** — playback omzeilt nu de live-mic-keten (EQ/compressor/AGC + 4×-boost) en gaat schoon op lijnniveau naar buiten voor Thetis en beide Yaesu-radio's; **playback naar de 2e Yaesu (FTX-1)** komt nu door; **Thetis TX-EQ wordt tijdens playback automatisch omzeild en daarna exact op de vorige stand hersteld**; een **play-volume-schuif** (0–2×) en een **zend-niveaumeter tijdens playback** toegevoegd; **RX-audio blijft hoorbaar tijdens TX** (de interne-speaker-mute hangt nu aan PTT-spike-protectie). |
| **2.4.1** | **Bugfix-patch.** Volledig interoperabel met v2.4.0 (wire-protocol VERSION 3 ongewijzigd; stock Thetis v2.10.3.15 volstaat). De **rotor (MCP2221A)** is nu vanuit een schone conf te koppelen (het koppel-scherm ontbrak — alleen tuners hadden er een); de **Settings-knop** verdwijnt niet meer na een MCP2221A-scan; **opgenomen audio afgespeeld via de radio** (TX-inject) speelt niet meer te langzaam/hakkelend (het TX-pad negeerde de opname-rate — speaker-playback was al goed); **FT-991A-geheugen 100–117** (PMS-kanalen) wordt nu ook ingelezen (stopte bij 099). FTX-1 ongewijzigd. |
| **2.4.0** | **Brede release: relay v2 (lage-latency UDP-audio + automatische TCP-terugval + beheer-dashboard), Yaesu SSB-via-USB + TX-compressor/AGC + clarifier, grote Android Yaesu-pariteit, uitgebreide verbindingsmonitoring en desktop-thema's.** Backwards-compatible met v2.3.x — wire-protocol VERSION 3 ongewijzigd; alle relay-toevoegingen zitten in de aparte relay-laag en de relay-tunnel, niet in het radio-protocol. **Relay** (self-hosted VPS, bron + Docker): station en client verbinden uitgaand (wss/TCP 443 voor besturing+spectrum, UDP 443 voor audio+PTT), werkt achter CGNAT/zonder port-forward. **Automatische UDP→wss-terugval** (make-before-break) met een **transport-indicator** op desktop én Android; UDP-tokens roteren periodiek. **Beheer-dashboard** met Argon2id-login, apparaat-/stationbeheer met verbruik/quota per device en een **database-backup**-knop. **Yaesu**: **SSB-zenden via USB-audio** (991A schakelt per PTT SSB MIC SELECT=REAR + PORT SELECT=USB; FTX-1 laat zijn interne auto-modulatiebron ongemoeid; auto-DATA alleen FM→DATA-FM, niet SSB; hybride per-PTT-routing + Exit), client-side **TX-compressor + AGC** per radio, **clarifier (RIT/XIT)**. **Android Yaesu-pariteit**: dual-radio-selector, volledig DSP-paneel, touch freq-tuning, interne ATU. **Databesparing mobiel** (geen Thetis-RX naar Yaesu-only clients; Yaesu-data alleen bij open venster). **Verbindingsmonitoring** fors uitgebreid (per-stream jitter/buffer/packets/loss + bandbreedte-uitsplitsing, desktop+Android). **Desktop-thema's** (Classic/Dark/Slate/Custom). Robuustheid: hoofdvenster-zelfherstel, jitter-buffer-resync bij stream-herstart, spectrum-bin-begrenzing als vangnet. **WebSDR herlaad-knop.** Geen Thetis-fork-wijziging — stock v2.10.3.15 volstaat. |
| **2.3.0** | **Synchrone AM (SAM-PLL) + AM auto-tune + instelbare TX-modulatiebandbreedte.** Backwards-compatible met v2.1.x/v2.2.0 — wire-protocol VERSION 3 ongewijzigd; nieuwe packet-/control-types (0x2A/0x2B, control 0x75–0x79) zijn additief en per-client gegate. **SAM** is nu een echte synchrone AM-demodulator (kritisch gedempte carrier-tracking PLL, WDSP `amd.c`-stijl, ±3 kHz vangbereik) i.p.v. pseudo-SAM; **auto-tune-to-carrier** laat de luisterfrequentie/VFO de draaggolf volgen via een twee-traps ruis-robuuste AFC. **Per-VRX audiobandbreedte** NB/WB/Auto, onafhankelijk per kanaal. **Instelbare TX-modulatiebandbreedte** in het desktop Thetis-tabblad (Volg RX of onafhankelijk low/high, 0–8 kHz), met symmetrische filter-mirror in AM/SAM/DSB/FM. Fixes: mode-wissel tijdens PTT niet meer doorgegeven (Thetis-desync-workaround), Follow-RX direct beschikbaar bij verbinden, automatisch terughalen van pop-out-vensters van een losgekoppelde monitor + handmatige "Recenter windows"-knop. Android ongewijzigd (geen VRX). Geen Thetis-fork-wijziging — stock v2.10.3.14+ volstaat. |
| **2.2.0** | **Virtuele ontvangers (VRX) + tweede Yaesu-radio (FT-991A + FTX-1).** Backward-compatible met v2.1.x — wire-protocol VERSION 3 ongewijzigd; de nieuwe packet-types (0x21–0x29) zijn additief en per-client gegate, dus v2.1.x-clients ontvangen ze nooit. **VRX1/VRX2** virtuele ontvangers uit de brede DDC-stroom via een FFT-channelizer, elk met eigen frequentie, mode (USB/LSB/AM/SAM/FM), filter, high-res spectrum/waterval en S-meter in één gezamenlijk popout-venster; NB/WB Opus-audio; per-bucket frequentiegeheugen + persistentie. **Dual-radio** tweede Yaesu-kanaal met model-autodetect (`ID;` 0670/0840), per-radio audio/CAT/geheugen, `RadioInfo`-paneelnaamgeving, **FTX-1 WIRES-X** EX-menu en een **software-squelch** (alleen FM-familie). **Schakelbare RX-bandbreedte** (Thetis + VRX + Yaesu, alleen ontvangst) en een **`#N` audio-device-index** voor identieke USB-codecs; dynamische WAV-opnamerate. Geïllustreerde VRX-leerboeken online (zie Documentatie). Pair met **Thetis fork PA3GHM TL2-4**; stock Thetis blijft ondersteund. |
| **2.1.0** | **Yaesu G-1000DXC rotor via MCP2221A, opt-in wideband Thetis RX, Amplitec reconnect, RX2 filter-fixes.** Backwards-compatible met v2.0.4 — wire-protocol ongewijzigd; 2.0.4-clients praten gewoon met 2.1.0-server (en omgekeerd). **Yaesu G-1000DXC rotor-backend** als 3e optie naast EA7HG en PstRotator: directe aansturing via Adafruit MCP2221A breakout (5 V mod), met soft-start/soft-stop ramp (1-200 %/s, default 50%), adaptive ADC poll-rate (30 Hz tijdens beweging / 1 Hz bij stilstand, mediaan-filter tegen 50/100 Hz netvoeding-ripple), kortste-route optie voor rotors met overlap-zone (max_deg > 360°), en kalibratie-wizard (Park CCW / Park CW). **Opt-in wideband Thetis RX** via fork-extensie — breekt geen stock-Thetis pad. **Amplitec 6/2 reconnect** na power-cycle + venster verschijnt ook bij offline-start (was: venster bleef onzichtbaar tot server-restart). **RX2 mode-switch filter-restore** (modulation-handler honoreert server filter-update bij modus-wissel) + per-channel filter-edge drag (RX1/RX2 drag-state gescheiden). **Yaesu EQ profile mic-gain persistence** (mic-slider wordt mee opgeslagen met band/treble); **scherpere TX resampler anti-alias filter**. **Modulaire multi-tuner wizard** met per-slot Add/Rename/Delete, classificatie-scan, inklapbaar MCP2221A-blok. **Status-paneel scroll-stabiliteit** (snapshot-cache bij lock-contentie; MCP2221A uitgeklapte sectie springt niet meer terug omhoog). UI-polish: chevron-labels op alle collapsible toggles, Settings-tab ScrollArea, Amplitec antenne-rename via right-click. Pair met **Thetis fork PA3GHM TL2-4** voor de volledige feature-set; stock Thetis blijft ondersteund. |
| **2.0.4** | **Bandbreedte-toolkit, preventieve TX-inhibit, power-cap, PstRotator.** Backwards-compatible met v2.0.3 — wire-protocol uitsluitend additief. **Preventieve RX-only TX-inhibit** via nieuw `rx_only_ex` TCI-commando (vereist Thetis-fork PA3GHM TL2-3): MOX/spatiebalk/hardware-PTT/VOX worden aan de bron geweigerd op een RX-only Amplitec-positie, niet reactief teruggeflipt; stock Thetis valt terug op de reactieve `ZZTX0` catch-all. **Reactieve RF-power cap per positie** met PA-eigen DriveDown (SPE + RF2K-S), mode-multipliers (SSB/CW × 1.0, AM × 0.5, FM/DIG × 0.4); rate-limit 1 s/stap — korte CW-bursts (<1 s) kunnen de reactieve cap passeren, preventieve dekking bestaat alleen op RX-only posities. **PstRotator UDP/XML rotor-backend** (host = numeriek IP-adres, geen DNS). **Server-tab bandbreedte-monitor** (Down/Up Kbit/s, klikbaar voor per-stream breakdown) — telt UDP application-payload bytes (de Windows-netwerkmeter leest ~1,5-2× hoger door IP/UDP/Ethernet-headers). **Per-client DX-spots opt-out** (Desktop + Android Settings), met server-side dedup (~90 Kbit/s broadcast storm → ~6 Kbit/s). **WebSDR favorites edit-toggle**. Server-log cleanup (PowerCap state-change-only + DXC reconnect 1-regel-per-cycle). |
| **2.0.3** | **Multi-tuner release + wire-protocol breaking change.** Twee fysieke StockCorner JC-4s/JC-3s tuners parallel via Adafruit MCP2221A USB-HID breakouts (vervangt de v2.0.2 serial-port RTS/CTS aansturing); per-tuner threshold + hysterese schuiven op de gele tune-status draad (1 MΩ + 1 MΩ deler, default 2.25 V / 0.50 V); board scan + serial programming UI; automatische USB-reconnect; inklapbaar MCP2221A-blok in het status-paneel. Daarnaast: S-meter herschreven met drie bronnen (Sig peak-hold, Avg true-mean, MaxBin), `rx_channel_sensors_ex` subscription, S9-frequency band shift; CTUN coupled-recenter + RX1/RX2 spectrum-mirror; MIDI client-side VFO-coalesce + auto-recenter handshake met de Thetis-fork; per-PA drive-snapshot persistence over proces-restart heen; collapsible window-states onthouden. **Wire-protocol u8 bumped van 2 → 3** (S-meter payload herschikt); v2.0.2-clients tegen v2.0.3-server (en omgekeerd) krijgen `ProtocolVersionMismatch` met gelocaliseerde melding ("Server is te oud" / "Client is te oud"). |
| **2.0.2** | **Log-spam hotfix:** server-side `DiversityPhaseEx`, `DiversityGainEx` en `DiversityGainMultiEx` notifications loggen nu alleen INFO bij echte value-change. Thetis pusht deze elke diversity-tick (~10-20 Hz), waardoor het server-log per sessie honderdduizenden regels telde. Functioneel gedrag en wire-protocol ongewijzigd — volledig interoperabel met v2.0.0 / v2.0.1. |
| 2.0.1 | **Connect-ervaring release:** first-run 4-stappen setup-wizard (Vind server → Wachtwoord → 2FA → Verbonden), mDNS local-network discovery (auto-vind servers op hetzelfde WiFi/LAN), 9 gedifferentieerde connect-states met platform-bewuste NL/EN hints, server Status-paneel (bind-adres, TCI-status, actieve clients met RTT/loss/jitter, audio-routing chips, recente connect-pogingen), slimme TciUnreachable hint (weet of Thetis draait, opstart of gestopt is), server-side RX2 audio-filter fix (geen fantoom CH2-stream meer als RX2 uit staat), Setup-wizard opnieuw starten knop. Wire-protocol ongewijzigd (VERSION = 2) — volledig interoperabel met 2.0.0. |
| 2.0.0 | **TL2 release:** Yaesu auto-DFM PTT-toggle (FM ↔ DATA-FM met memory-restore), server-side CTUN auto-recenter, live diversity null-circle broadcast (Smart/Ultra), filter-preset push (F1..VAR2/NONE), per-RX DDC sample rate (48..1536 kHz), `tci_caps_ex` capability broadcast, DX cluster click-to-tune, SWR display in TX meter, CW keyer + macros over TCI, single-TCI-only architectuur (geen aparte CAT meer), wire-protocol VERSION = 2 |
| 1.0.0 | Eerste publieke release op `cjenschede/ThetisLink` |
| 0.5.0 | Yaesu FT-991A ondersteuning, Bluetooth headset (Android), diversity ontvangst fix, TCI besturingselementen, RF2K-S reset, PTT modi, DX Cluster |
| 0.4.9 | Wideband Opus TX, device switch fix |
| 0.4.2 | Configureerbaar FFT formaat, dynamische spectrum bins, Android power knop fix |
| 0.4.1 | WebSDR/KiwiSDR integratie, frequentie sync, TX spectrum auto-override |
| 0.4.0 | TCI WebSocket, waterval click-to-tune Android |
| 0.3.2 | MIDI controller ondersteuning, PTT toggle met LED, Mic AGC |
| 0.3.1 | Band geheugen, FM filter fix, macOS client |
| 0.3.0 | Volledige RX2/VFO-B ondersteuning, DDC spectrum+waterval |





