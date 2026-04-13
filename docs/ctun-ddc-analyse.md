# Effect van de CTUN-knop in Thetis v2.10.3.12 op DDC-netwerkstreams en waarom CTUN de frequenti-offset in gecapturede DDC-data lijkt te veranderen

## Executive summary

CTUN in Thetis (intern aangeduid als *ClickTuneDisplay*, UI-element `chkFWCATU`) schakelt Thetis om van “één frequentiereferentie” (VFO ≈ DDC-center) naar een tweedelig tuningsmodel waarin **de hardware‑DDC op de *panadapter‑centerfrequentie* blijft** en Thetis vervolgens **een extra softwarematige oscillator-offset** (*RXOsc*, in Hz) toepast om de daadwerkelijke ontvangstfrequentie binnen die DDC-band te kiezen. Dit staat direct in de Thetis-broncode: bij CTUN **uit** wordt `FWCDDSFreq` (de frequentie die via NetworkIO/ChannelMaster in het Protocol‑2 High Priority pakket als DDC0 frequentie/phaseword uitgaat) gezet op `rx_freq`; bij CTUN **aan** wordt `FWCDDSFreq` gezet op (een variant van) `CentreFrequency` en wordt de resterende offset in de DSP als `RXOsc` beheerd. citeturn34view0turn35view0turn42view0

Daardoor is een “rauwe” DDC0 I/Q stream over het netwerk **altijd gecentreerd rond de DDC0 centerfrequentie** die in Protocol‑2 (High Priority packet, bytes 9–12) wordt ingesteld, terwijl de Thetis‑VFO en de voor demodulatie gebruikte oscillator-offset bij CTUN aan in belangrijke mate losgekoppeld zijn. In captures zie je dan een schijnbaar “onverwachte” offsetverandering omdat je (a) de **verandering van DDC-center** niet meeneemt, of (b) CTUN tijdens tunen de **CentreFrequency dynamisch verschuift** (scroll/recenter) om binnen DSP-sampleruimte en displaymarges te blijven, of (c) CW‑pitch/RIT in CTUN-modus anders verdeeld wordt tussen hardware‑tuning en software‑offset. citeturn35view0turn42view0turn16view3turn39view0

De meest robuuste verklaring voor jouw observatie (“frequentieverschil zichtbaar als ik CTUN aan zet en de frequentie in Thetis aanpas”) is daarom: **je meet de frequentie in de DDC‑stream relatief t.o.v. een impliciete referentie (vaak de Thetis‑VFO), terwijl bij CTUN aan de DDC‑stream relatief t.o.v. `CentreFrequency` gecentreerd blijft en `CentreFrequency` bovendien door Thetis kan meebewegen (scroll) tijdens tunen**; dat levert veranderende offsets in de capture op, zonder dat er sprake hoeft te zijn van foutieve data. citeturn42view0turn35view0turn39view0

---

## Bronnen, scope en terminologie

### Primaire bronnen die daadwerkelijk de datastroom definiëren

De end‑to‑end keten “CTUN → protocolvelden → DDC I/Q data” is te reconstrueren met alleen primaire lagen:

Protocol 2 specificatie voor (a) DDC0 frequentie (in High Priority), (b) DDC packet format, (c) DDC sample rates en (d) byte order/phaseword-selectie. citeturn18view3turn39view0turn40view1turn38view3

Thetis v2.10.3.12 broncode waar CTUN en tuninglogica worden uitgevoerd (`console.cs`) en waar de frequentie die naar “firmware” gaat (`FWCDDSFreq`) verschillend wordt ingevuld afhankelijk van CTUN. citeturn34view0turn35view0turn42view0

Thetis/ChannelMaster implementatie van Protocol‑2 pakketten: `netInterface.c` (export `SetVFOfreq`), `network.c` (bouwt en verstuurt o.a. High Priority packets). citeturn28view0turn16view3turn13view2

Thetis NetworkIO layer (`NetworkIO.cs` + imports) die frequenties óf als Hz óf als phaseword doorgeeft; voor Ethernet/Protocol‑2 wordt expliciet phaseword gebruikt met de bekende 2^32‑schaalformule. citeturn25view0turn26view0turn18view3

### Relevante contextbronnen

Thetis is in documentatie en release‑context gepositioneerd als opvolger van PowerSDR, met focus op “Protocol 2” radio interface. citeturn50search6turn32search6

De ANAN‑7000DLE‑familie wordt in publieke documentatie beschreven als een radio waarbij ADC‑samples breedbandig worden genomen en receivers (DDCs) in FPGA worden gedecimeerd voor Ethernet-transport naar de PC—dat past bij het Protocol‑2 model. citeturn43search1

### Terminologie die je captures anders laat interpreteren

In dit rapport gebruik ik consequent de volgende definities:

`VFOAFreq` (“VFO”, MHz) = de door gebruiker ingestelde “receive frequency” zoals zichtbaar in Thetis.

`CentreFrequency` (MHz) = de panadapter centerfrequentie (display‑center) die bij CTUN aan in de praktijk van de VFO kan loskomen en ook gescrolld kan worden. citeturn42view0

`RXOsc` (Hz) = softwarematige meng-oscillator-offset die Thetis in de DSP gebruikt om binnen het DDC‑baseband een andere subfrequentie te demoduleren. In code wordt dit o.a. in `radio.GetDSPRX(...).RXOsc` gezet. citeturn42view0

`FWCDDSFreq` (MHz) = de frequentie die Thetis als “DDS/DDC tuning” naar de radio stuurt via NetworkIO/ChannelMaster; Protocol‑2 vertaalt dit uiteindelijk naar DDC0 frequentie/phaseword in het High Priority packet. citeturn35view0turn25view0turn28view0turn16view3

---

## Wat CTUN in Thetis v2.10.3.12 concreet verandert aan DDC-streams

### CTUN schakelt het tuningsmodel om en forceert een herberekening

De CTUN‑UI handler `chkFWCATU_CheckedChanged` zet intern `ClickTuneDisplay` (CTUN) en triggert vervolgens expliciet `txtVFOAFreq_LostFocus(...)`, waardoor alle afgeleide grootheden opnieuw worden gezet (CentreFrequency, RXOsc, FWCDDSFreq, enz.). citeturn34view0

Belangrijk detail: de handler zet bij inschakelen CTUN tijdelijk RIT uit en daarna terug aan, en roept dan pas de tuning-update aan. Dat is relevant als je offsets ziet die exact gelijk zijn aan de RIT‑waarde, omdat CTUN‑logica RIT vaak als DSP‑offset (RXOsc) behandelt in plaats van als hardware-DDC retune. citeturn34view0turn42view0

### CTUN bepaalt of `FWCDDSFreq` (hardware DDC tuning) de VFO volgt of de CentreFrequency volgt

In de kern van `txtVFOAFreq_LostFocus` zie je het beslispunt:

Bij CTUN uit (`!_click_tune_display`) wordt `FWCDDSFreq` gezet op `rx_freq` (“update rx freq”). citeturn35view0

Bij CTUN aan (`_click_tune_display`) wordt `FWCDDSFreq` gezet op `CentreFrequency` (met extra CW‑pitch correctie in CW‑modi). citeturn35view0turn34view1

Dit is de meest directe reden waarom de **DDC I/Q stream** in een capture anders “gecentreerd” lijkt bij CTUN aan: de radio stuurt DDC0 I/Q rond **`FWCDDSFreq`**, niet rond `VFOAFreq`. citeturn35view0turn39view0

### CTUN gebruikt `RXOsc` en kan `CentreFrequency` verschuiven tijdens tunen

Als CTUN aan is, probeert Thetis tuning binnen het zichtbare spectrum en binnen de beschikbare sampleband te houden. Dat gebeurt door:

`RXOsc` (hier: `rx1_osc`) te beperken t.o.v. de samplerate (`sample_rate_rx1`) en een veiligheidsfactor (0.92) — als de offset buiten de “IF/sample area” komt, corrigeert Thetis frequentie/offset. citeturn42view0

Bij “tunen aan de rand” van de panadapter (marges) de CentreFrequency te scrollen (*CentreFrequency -=/+ adjustFreq*) om het signaal/filters binnen het display te houden. citeturn42view0

Op “grote sprongen” (freqJumpThresh) te recenteren door `CentreFrequency = freq` en `rx1_osc = 0.0`. citeturn42view0

De DSP‑offset wordt uiteindelijk toegepast via `radio.GetDSPRX(0,0).RXOsc = rx1_osc`. citeturn42view0

Gevolg voor captures: ook als jij alleen “de frequentie aanpast” in Thetis, kan CTUN ervoor zorgen dat **CentreFrequency mee verschuift** (scroll/recenter). En omdat `FWCDDSFreq` bij CTUN aan aan `CentreFrequency` hangt, **retunet de hardware DDC-center** en verschuift de gehele baseband in de DDC stream. citeturn42view0turn35view0

---

## Protocol 2 veldmapping: wat Thetis verstuurt en wat de 7000DLE firmware daarmee doet

### Control-plane: welke UDP poorten en pakketten bepalen DDC instellingen

In Thetis/ChannelMaster is duidelijk te zien dat Protocol‑2 via UDP met vaste “default” poorten werkt. Tijdens start stuurt Thetis o.a. General, DDC Specific (CmdRx) en High Priority. citeturn13view0turn18view0

Belangrijkste stromen in jouw vraag (DDC0):

General packet (PC→hardware) zet o.a. de “base ports” zoals DDC0 base port (default 1035) en flags als “phase word” selectie. citeturn13view3turn38view1turn16view2turn38view3

DDC Specific packet (PC→hardware, in Thetis `CmdRx`) zet per DDC de sampling rate en sample size (bijv. DDC0 sampling rate als 48/96/192/384/768/1536 ksps; sample size default 24 bits). citeturn40view1turn16view3

High Priority packet (PC→hardware) bevat de DDC0 frequentie/phaseword in bytes 9–12. In Thetis wordt dit veld gevuld uit `prn->rx[0].frequency`. citeturn18view3turn16view3turn28view0turn25view0

### Data-plane: hoe DDC0 I/Q over het netwerk wordt verzonden

Volgens de Protocol‑2 specificatie wordt DDC0 I/Q standaard verzonden vanaf UDP source port 1035 (DDC1 = 1036, etc.) en bevat elk packet o.a.:

32‑bit sequence number (per port) citeturn39view0turn38view1  
64‑bit timestamp (VITA‑49 sample count timestamp concept; fysieke relevantie hangt af van hardware/enablement) citeturn39view0turn38view3  
bits‑per‑sample (FPGA code typisch 24 bits) citeturn39view0turn40view1  
samples‑per‑frame en daarna interleaved I/Q samples als signed 2’s complement. citeturn39view0turn38view3

De spec waarschuwt bovendien dat de I/Q-semantiek (welk kanaal “I” is) tot een gespiegeld spectrum kan leiden, met als remedie I en Q omwisselen in de verwerking. citeturn39view0

### De cruciale brug: hoe CTUN uiteindelijk in het Protocol‑2 High Priority veld belandt

De keten in Thetis is technisch hard:

`FWCDDSFreq` (console tuninglogica) wordt afhankelijk van CTUN gevuld met `rx_freq` (CTUN uit) of `CentreFrequency` (CTUN aan, plus CW‑pitch correctie). citeturn35view0turn34view1

`FWCDDSFreq` leidt tot een call naar `NetworkIO.VFOfreq(...)`. In `NetworkIO.VFOfreq` wordt voor Ethernet/Protocol‑2 een **phaseword** berekend (`Freq2PW`) met de bekende formule (2^32 * f / 122.88 MHz) en via `SetVFOfreq` aan ChannelMaster doorgegeven. citeturn25view0turn18view3

`SetVFOfreq` in ChannelMaster schrijft de waarde naar `prn->rx[id].frequency` en triggert `CmdHighPriority()` om het direct te versturen. citeturn28view0turn13view2

`CmdHighPriority()` zet bytes 9–12 van het High Priority packet naar `prn->rx[0].frequency` (of een TX‑variant bij PureSignal/PTT), en verzendt naar UDP port 1027 (default). citeturn16view3turn13view2

Volgens de Protocol‑2 spec is dat veld precies de “DDC0 Frequency/Phase Word”. citeturn18view3

### Tabel: mapping tussen CTUN-staat en wat jij in DDC0 capture ziet

| Grootheid | CTUN uit (ClickTuneDisplay = false) | CTUN aan (ClickTuneDisplay = true) | Effect in DDC0 capture |
|---|---|---|---|
| Hardware DDC0 center | `FWCDDSFreq = rx_freq` citeturn35view0 | `FWCDDSFreq = CentreFrequency` (+ CW pitch in CW modes) citeturn35view0turn34view1 | Baseband in de DDC stream is relatief t.o.v. deze center; als jij VFO als referentie neemt, ontstaan “offset verrassingen”. citeturn39view0 |
| Software offset in DSP | RXOsc ≈ 0 in normale tuning | RXOsc (`rx1_osc`) wordt actief gebruikt en begrensd | De gedemoduleerde frequentie kan “meelopen” terwijl de DDC stream center gelijk blijft. citeturn42view0 |
| CentreFrequency gedrag tijdens tunen | Volgt doorgaans de VFO (display-center = VFO) | Kan scrollen/recenter (marges + sample-area checks) | Bij scroll/recenter verandert de DDC center → de hele capture “schuift”. citeturn42view0turn35view0 |
| CW pitch / RIT verdeling | Grotere kans dat CW/RIT in `rx_freq` (hardware center) zit | CW pitch wordt expliciet in `dTmpFreq` voor HW gezet; RIT wordt als RXOsc-correctie toegepast | Offsetverschillen die exact CW pitch of RIT volgen zijn verklaarbaar. citeturn35view0turn42view0 |

---

## Protocol 1 versus Protocol 2 voor DDC/wideband gedrag

Deze vergelijking is vooral nuttig om misinterpretatie bij captures te voorkomen als je tooling “Protocol‑1 aannames” heeft.

### Protocol 1 (Metis/HPSDR USB frames over UDP)

Metis gebruikt UDP port 1024 voor discovery/start/stop en data, en verstuurt payloads met o.a. `<0xEFFE>`, endpoint-id, sequence number (big-endian) en vervolgens **2× 512-byte HPSDR USB frames**. citeturn52view0

De onderliggende HPSDR USB-protocol frames (512 bytes) beginnen met sync bytes (0x7F 0x7F 0x7F), bevatten C&C bytes en daarna I/Q samples. I/Q is 24-bit (3 bytes per I en 3 per Q) en samplerates zijn 48/96/192/384kHz; NCO-frequenties en andere control info zitten in de C&C-structuur. citeturn53view0

“Wideband/bandscope” in dit model gebeurt via EP4 met blokken raw ADC samples (o.a. bedoeld voor bandscope). citeturn53view0

### Protocol 2 (openHPSDR Ethernet Protocol, DDC/DUC model)

Protocol‑2 splitst control plane op in afzonderlijke UDP “control packets” (General, DDC specific, High Priority) en data plane in afzonderlijke UDP streams per DDC (bijv. DDC0 source port 1035). citeturn40view0turn38view1turn39view0

DDC sample rate is per DDC_attach te selecteren (48/96/192/384/768/1536 ksps) via DDC Specific. citeturn40view1

De DDC centerfrequentie wordt via High Priority als frequentie/phaseword gestuurd (met keuze in General packet of het om Hz of phaseword gaat; big-endian). citeturn18view3turn38view3

### Tabel: snelle vergelijking op jouw meetpunt (DDC-center en dataformat)

| Aspect | Protocol 1 | Protocol 2 |
|---|---|---|
| Control-flow | Control info (incl. NCO freq) zit in 512B frames (C&C), verzonden in Metis UDP payloads | Control info gescheiden in General/DDC Specific/High Priority packets citeturn53view0turn40view0turn18view3 |
| DDC centerfrequentie veld | “NCO frequencies” als onderdeel van C&C round-robin (conceptueel) citeturn53view0 | High Priority bytes 9–12 = DDC0 freq/phaseword citeturn18view3turn16view3 |
| DDC I/Q transport | In USB frame payload (I2 I1 I0 Q2 Q1 Q0 …) citeturn53view0 | Los UDP stream per DDC; DDC0 default source port 1035; header + I/Q samples citeturn39view0turn38view1 |
| Endianness | Metis en control sequence big-endian citeturn52view0 | Network byte order big-endian (tenzij DFC/LE optioneel) citeturn38view3 |
| Wideband | EP4 raw ADC blocks (bandscope) citeturn53view0 | Wideband UDP packet (default port 1027 voor ADC0 wideband data) en apart DDC packets citeturn39view0turn38view1 |

---

## Debugging, packet capture aanpak en mitigaties

### Wat je minimaal moet capturen om CTUN-effect hard te bewijzen

Om jouw “frequentie offset in DDC data” correct te verklaren, moet je **niet alleen DDC0 data capturen**, maar ook de **High Priority packets** van host→radio (die DDC0 centerfrequentie/phaseword bevatten). In Thetis/ChannelMaster wordt High Priority naar UDP port 1027 gestuurd. citeturn13view2turn18view3turn16view3

Capture daarom tegelijk:

Host → radio: High Priority (udp.dstport==1027 of udp.port==1027)  
Radio → host: DDC0 I/Q (udp.srcport==1035) citeturn38view1turn39view0

Optioneel (handig voor volledigheid): General (1024) en DDC Specific (1025) zodat je ziet of phaseword aan staat en welke samplerates ingesteld zijn. citeturn40view0turn16view2turn38view3

### Exacte Wireshark filters

Neem je radio-IP als `<RADIO_IP>`.

High Priority (host → radio):
- `ip.dst == <RADIO_IP> && udp.dstport == 1027`

DDC0 I/Q (radio → host):
- `ip.src == <RADIO_IP> && udp.srcport == 1035`

DDC specific (host → radio, samplerate/bitdepth):
- `ip.dst == <RADIO_IP> && udp.dstport == 1025` citeturn40view0turn13view0

General (host → radio, phaseword-selectie):
- `ip.dst == <RADIO_IP> && udp.dstport == 1024` citeturn40view0turn38view3

### Waar in de bytes je moet kijken

High Priority DDC0 centerfrequentie:
- Bytes 9–12 bevatten DDC0 Frequency/Phase Word (big-endian). citeturn18view3turn16view3turn38view3

DDC0 data packets (UDP source port 1035):
- Bytes 0–3: sequence number
- Bytes 4–11: timestamp
- Bytes 12–13: bits per sample
- Bytes 14–15: samples per frame
- Daarna I/Q samples (2’s complement), met de noot dat I/Q interpretatie kan leiden tot gespiegelde FFT en dat omwisselen soms nodig is. citeturn39view0turn38view3

### Voorbeeld: simpele hex-check van het DDC0 phaseword

Gebruik de Protocol‑2 fasewoordformule (ook in Thetis zelf aanwezig):  
`phaseword = floor(2^32 * f_Hz / 122880000)` citeturn18view3turn25view0

Voorbeeld (14.074 MHz) levert phaseword `0x1D522222`, dus bytes 9–12 zouden er zo uit kunnen zien:

```text
HighPriority payload (bytes 9..12):  1D 52 22 22
```

Als je CTUN aanzet en je ziet dat bytes 9–12 “meelopen” met `CentreFrequency` in plaats van met VFO, dan is je offsetverschil in DDC capture verklaard: je baseband is gecentreerd rond de door CTUN gekozen `FWCDDSFreq` i.p.v. rond VFO. citeturn35view0turn42view0turn16view3turn39view0

### Correlatie-experiment dat je in 2 minuten kunt doen

1. Zet een stabiele draaggolf in beeld (of gebruik een bekende marker).  
2. Start capture met bovenstaande filters (High Priority + DDC0 (1035)).  
3. CTUN **uit**: draai VFO stapjes van +1 kHz.  
4. CTUN **aan**: herhaal, en zorg dat je ook “aan de rand” van het display komt zodat CTUN de centre kan scrollen/recenter.  

Wat je dan typisch ziet:

Bij CTUN uit verandert bytes 9–12 (DDC0 center) bij elke stap mee met `rx_freq` (want `FWCDDSFreq = rx_freq`). citeturn35view0turn16view3

Bij CTUN aan blijft bytes 9–12 vaak gelijk zolang Thetis alleen RXOsc verplaatst; maar zodra CentreFrequency scrolt/recentered (door sample-area/marge checks) zie je bytes 9–12 “springen” → precies dan schuift ook je gecapturede baseband offset. citeturn42view0turn35view0turn16view3

### Relevante code-locaties voor jouw analyse en eventuele patches

Omdat `console.cs` (2.22 MB) in GitHub UI niet inline rendert, geef ik hier “approximate line numbers” (op basis van de v2.10.3.12 tag) plus de functienamen zodat je ze lokaal exact kunt bevestigen:

CTUN UI handler:
- `Project Files/Source/Console/console.cs` → `chkFWCATU_CheckedChanged(...)` (≈ L44339) roept `txtVFOAFreq_LostFocus(...)` aan na het zetten van `ClickTuneDisplay`. citeturn34view0

Tuningkern waar CTUN hardware/DDC center beïnvloedt:
- `console.cs` → `txtVFOAFreq_LostFocus(...)` (≈ L32240) bevat:
  - `if (!_click_tune_display) FWCDDSFreq = rx_freq;`
  - `if (_click_tune_display) { ... dTmpFreq = CentreFrequency ... (CW pitch) ... FWCDDSFreq = dTmpFreq; }` citeturn35view0turn34view1  
  - limitering/scroll/recenter rond `sample_rate_rx1 * 0.92` en aanpassing van `CentreFrequency` en `rx1_osc` (RXOsc). citeturn42view0

Protocol‑2 uitsturing van die frequentie:
- `Project Files/Source/Console/HPSDR/NetworkIO.cs` → `VFOfreq(...)` (≈ L2711) rekent voor Ethernet om naar phaseword (Freq2PW) en roept `SetVFOfreq`. citeturn25view0turn18view3
- `Project Files/Source/Console/HPSDR/NetworkIOImports.cs` → `[DllImport] SetVFOfreq(int id, int freq, int tx)` (≈ L1224). citeturn26view0
- `Project Files/Source/ChannelMaster/netInterface.c` → `SetVFOfreq(...)` (rond L2972) schrijft naar `prn->rx[id].frequency` en triggert `CmdHighPriority()`. citeturn28view0
- `Project Files/Source/ChannelMaster/network.c` → `CmdHighPriority()` schrijft `prn->rx[0].frequency` naar bytes 9–12 en stuurt naar UDP port 1027. citeturn16view3turn13view2

### Aanbevolen mitigaties zonder codewijziging

Als je doel is: “DDC raw capture moet voorspelbaar zijn t.o.v. de VFO”, dan zijn de meest praktische opties:

CTUN uit tijdens captures: dan is `FWCDDSFreq = rx_freq` en centre volgt VFO, waardoor je baseband interpretatie meestal triviaal is. citeturn35view0

Bij CTUN aan: log/lees altijd de **DDC0 center** uit High Priority bytes 9–12 en interpreteer de FFT-frequentieas als `f_abs = f_center + f_bin`, waarbij `f_center` uit High Priority komt. Dit is het “Protocol‑2 correcte” model. citeturn18view3turn39view0

Vermijd situaties waarin CTUN de centre moet verschuiven: vergroot de samplerate (DDC Specific) of verminder zoom/filterbreedte zodat de passband comfortabel binnen de display/sampleruimte blijft; anders gaat de code expliciet scrollen/recenter. citeturn40view1turn42view0

### Minimale codewijzigingen als je CTUN‑capturing “minder verrassend” wilt maken

Deze aanbevelingen zijn “compatibility hacks”: ze veranderen gedrag, dus test ze zorgvuldig.

Optie A: maak CTUN puur “client-side” voor captures  
Doel: DDC0 center volgt altijd `rx_freq` (zoals CTUN uit), terwijl CTUN alleen display/DSP offset doet.

In `txtVFOAFreq_LostFocus`, vervang in de `_click_tune_display` branch de assignment `FWCDDSFreq = dTmpFreq` door `FWCDDSFreq = rx_freq` (of `FWCDDSFreq = freq` afhankelijk van of je RIT/CW in hardware wilt). Je verliest daarmee echter het ontwerpprincipe dat DDC center = panadapter center bij CTUN. De huidige intentie blijkt juist dat CTUN hardware center op `CentreFrequency` wil houden. citeturn35view0turn42view0

Optie B: haal CW-pitch correctie uit de hardwarecenter in CTUN-mode  
Als jouw “offsetsprong” exact gelijk is aan CW pitch, komt dat doordat Thetis bij CTUN aan CW pitch optelt/aftrekt in `dTmpFreq` vóór `FWCDDSFreq` gezet wordt. citeturn34view1turn35view0  
Je kunt dan die CW‑switchcase weghalen en CW pitch volledig via RXOsc/demod doen. Dit maakt captures “RF-centre correcter” maar kan invloed hebben op hoe CW zero-beat/sidetone in UI werkt.

Optie C: voeg een “capture mode” toe: exporteer metadata  
Een niet-invasieve maar effectieve oplossing is: bij starten van een DDC recording schrijf je naast de samples ook:
- actuele DDC0 centerfrequentie (uit `prn->rx[0].frequency` / High Priority bytes 9–12),
- samplerate (DDC Specific),
- RXOsc (DSP offset),
zodat postprocessing altijd exact weet hoe de signaalas moet worden geïnterpreteerd. Dit sluit aan bij Protocol‑2 scheiding van center versus offset. citeturn18view3turn39view0turn42view0

---

## Mermaid flowchart van de CTUN → Protocol‑2 control → DDC stream → FFT keten

```mermaid
flowchart TD
  U[Gebruiker: CTUN toggle / VFO tuning] --> C1[console.cs: chkFWCATU_CheckedChanged]
  C1 -->|zet ClickTuneDisplay| CTUN[CTUN staat gewijzigd]
  CTUN --> C2[console.cs: txtVFOAFreq_LostFocus]

  C2 -->|CTUN uit| HW1[FWCDDSFreq = rx_freq]
  C2 -->|CTUN aan| HW2[FWCDDSFreq = CentreFrequency (+ CW pitch)]
  C2 --> DSP1[Zet RXOsc (rx1_osc) en begrens binnen sample area]
  DSP1 -->|scroll/recenter nodig| CF[CentreFrequency schuift]
  CF --> HW2

  HW1 --> N1[NetworkIO.VFOfreq: Hz -> phaseword (Protocol 2)]
  HW2 --> N1
  N1 --> CM1[ChannelMaster SetVFOfreq: prn->rx[0].frequency]
  CM1 --> HP[CmdHighPriority: bytes 9..12 = DDC0 phaseword]
  HP -->|UDP dstport 1027| RADIO[7000DLE MKII firmware: DDC0 center retune]

  RADIO -->|UDP srcport 1035| DDC[DDC0 I/Q packets: seq/timestamp/bits/samples + IQ]
  DDC --> FFT[Thetis FFT/panadapter pipeline]
  FFT --> UI[Weergave spectrum + audio demod (met RXOsc)]
```

---

## CTUN en RX2/DDC3 (meerdere ontvangers)

### CTUN geldt globaal

De CTUN-knop in Thetis is een globale instelling die voor alle ontvangers geldt. Er is geen aparte CTUN per VFO/ontvanger. Het CAT-commando `ZZCT` retourneert de globale CTUN status.

### Effect op DDC3 (RX2) centerfrequentie

Het CTUN-mechanisme werkt identiek voor RX2 als voor RX1, maar met andere variabelen:

| Grootheid | CTUN uit | CTUN aan |
|-----------|----------|----------|
| DDC2 center (RX1) | `FWCDDSFreq = rx_freq` (VFO-A) | `FWCDDSFreq = CentreFrequency` (RX1 panadapter center) |
| DDC3 center (RX2) | Volgt VFO-B | Bevriest op RX2 panadapter center |
| HP slot 2 (DDC2) | Verandert bij elke VFO-A tune | Stabiel totdat re-center nodig is |
| HP slot 3 (DDC3) | Verandert bij elke VFO-B tune | Stabiel totdat re-center nodig is |

### Praktische gevolgen voor ThetisLink spectrum capture

Bij RX2 DDC3 capture in ThetisLink:

**CTUN uit:**
- DDC3 center volgt VFO-B → spectrum altijd gecentreerd op VFO-B
- Elke VFO-B wijziging verplaatst het hele spectrum (zichtbaar als "sprong" in waterfall)
- HP packet slot 3 phaseword verandert mee

**CTUN aan:**
- DDC3 center blijft vast → spectrum stabiel (geen sprongen)
- VFO-B marker (rode lijn) beweegt over het spectrum
- HP packet slot 3 phaseword blijft gelijk zolang geen re-center nodig is
- Bij grote VFO-B sprong: Thetis doet re-center → DDC3 center springt → spectrum verschuift

**ThetisLink implementatie:**
- `Rx2SpectrumProcessor::set_vfo_freq(freq_hz, ctun)` ontvangt VFO-B + CTUN status
- `Rx2SpectrumProcessor::set_ddc_center(freq_hz)` ontvangt HP slot 3 phaseword
- HP packet data heeft altijd voorrang (meest nauwkeurig en snelst beschikbaar)
- Bij CTUN aan + geen HP data: DDC center wordt bevroren op laatste bekende waarde
- Display offset berekening: `vfo_offset = vfo_b_freq - ddc3_center_freq`

### Waarom de offset bij RX2 anders kan zijn dan bij RX1

Omdat VFO-A en VFO-B onafhankelijk van elkaar bewegen, kunnen de offsets ten opzichte van hun DDC centers anders zijn:
- RX1: offset = VFO-A - DDC2_center
- RX2: offset = VFO-B - DDC3_center

Bij split VFO (verschillende banden) kunnen deze offsets sterk verschillen. Bij CTUN aan kan RX1 op 14.345 MHz staan met DDC2 center op 14.350 MHz (offset -5 kHz), terwijl RX2 op 7.073 MHz staat met DDC3 center op 7.070 MHz (offset +3 kHz).

---

## Assumpties en expliciete onzekerheden

Firmwareversie/FPGA image van jouw ANAN‑7000DLE MKII is niet gespecificeerd. Dit rapport neemt aan dat de radio zich gedraagt conform openHPSDR Protocol‑2 (DDC0 port 1035, High Priority bytes 9–12 als phaseword). citeturn39view0turn38view1turn18view3

Ik ga ervan uit dat Thetis in jouw setup daadwerkelijk “Ethernet/Protocol‑2 mode” draait (niet USB), omdat `NetworkIO.VFOfreq` dan phasewords verstuurt; bij USB zou hij Hz versturen. citeturn25view0

De officiële Apache‑Labs PDF’s voor Thetis manual en 7000DLE‑MKII user guide waren via deze tool niet rechtstreeks te openen (HTTP fetch gaf “(400) OK”), daarom citeer ik beperkte informatie uit publieke index-snippets van die URLs als context, en baseer ik de technische specificatie primair op openHPSDR Protocol‑2 en Thetis broncode. citeturn50search6turn43search1turn39view0turn35view0

Nederlandstalige primaire documentatie over CTUN/Protocol‑2 is niet aangetroffen binnen de gebruikte bronnen; dit rapport is daarom gebaseerd op Engelstalige specificaties en broncode. citeturn50search6turn39view0turn42view0

---

## Bronlinks

Onderstaande links zijn dezelfde bronnen die in de citations gebruikt zijn (handig om direct te openen):

```text
Thetis v2.10.3.12 repo (release context):
- https://github.com/ramdor/Thetis

Thetis v2.10.3.12 console.cs (raw):
- https://raw.githubusercontent.com/ramdor/Thetis/v2.10.3.12/Project%20Files/Source/Console/console.cs

Thetis NetworkIO (Protocol/phaseword):
- https://github.com/ramdor/Thetis/blob/v2.10.3.12/Project%20Files/Source/Console/HPSDR/NetworkIO.cs
- https://github.com/ramdor/Thetis/blob/v2.10.3.12/Project%20Files/Source/Console/HPSDR/NetworkIOImports.cs

Thetis ChannelMaster:
- https://github.com/ramdor/Thetis/blob/v2.10.3.12/Project%20Files/Source/ChannelMaster/netInterface.c
- https://github.com/ramdor/Thetis/blob/v2.10.3.12/Project%20Files/Source/ChannelMaster/network.c

openHPSDR Ethernet Protocol (Protocol 2, v3.6 PDF):
- https://ad0es.net/dfcSDR/fpga/files/openHPSDR_Ethernet_Protocol_v3.6.pdf

Metis protocol (Protocol 1 over UDP):
- https://openhpsdr.org/downloads/documents/Metis/Documentation/Archive/Metis-%20How%20it%20works_V1.23.pdf

HPSDR USB Data Protocol (basis voor Protocol 1 frames):
- https://openhpsdr.org/support/Ozy/USB_protocol_V1.57.pdf

Apache Labs context (manual snippets; PDF fetch-blocked in tool):
- https://apache-labs.com/public/storage/download_file/1756364911_1020_Thetis-manual_1.0.pdf
- https://apache-labs.com/public/storage/download_file/1756365391_1016_7000DLE-MKII-User-guide.pdf
```