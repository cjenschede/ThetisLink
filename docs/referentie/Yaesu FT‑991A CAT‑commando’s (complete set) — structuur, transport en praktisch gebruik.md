# Yaesu FT‑991A CAT‑commando’s (complete set) — structuur, transport en praktisch gebruik

## Executive summary

De Yaesu FT‑991A gebruikt een “new‑CAT” (tekstgebaseerd) protocol waarbij elk commando uit **twee letters**, eventuele **vaste‑lengte parameters** en een **terminator `;`** bestaat. Een minimaal voorbeeld is `FA145500000;` om VFO‑A op 145,500,000 Hz te zetten; de radio antwoordt bij een read meestal met hetzelfde commando plus de gevraagde waarde. citeturn3view0turn22view0

Bij USB‑aansluiting installeert de Yaesu/Silicon‑Labs driver **twee virtuele COM‑poorten**: een **Enhanced COM Port** (bedoeld voor **CAT communicatie** en firmware‑update) en een **Standard COM Port** (bedoeld voor **TX‑controls zoals PTT/CW keying/digital mode operation**, typisch via RTS/DTR/FSK‑lijnen). Voor de **CAT‑tekstcommando’s** in deze rapportage is de **Enhanced COM Port** in de praktijk de juiste keuze. citeturn29view0

De officiële FT‑991A CAT‑handleiding geeft een **commando‑lijst van 91 commando’s** (AB…ZI) met per commando of het **Set**, **Read**, **Answer** en/of **AI (Auto Information)** ondersteunt. citeturn3view0  
Belangrijke uitzonderingen/vereisten uit de officiële documentatie zijn o.a. dat **OS (repeater shift)** “alleen activeerbaar is in FM‑mode” citeturn16view0turn27view2 en dat **PS (power switch)** een **timing‑procedure met ‘dummy data’** vereist. citeturn27view1turn15view0

Er zijn relevante verschillen t.o.v. de FT‑991 (zonder A): het **ID‑antwoord** is **0570** bij FT‑991 en **0670** bij FT‑991A. citeturn24view1turn13view0 Ook verschilt o.a. het **IF‑SHIFT bereik** (FT‑991: ±1000 Hz; FT‑991A: ±1200 Hz) citeturn24view1turn13view1 en de **EX menu‑range** (FT‑991: 001–151; FT‑991A: 001–153). citeturn25view0turn7view0

## Transport, framing en protocolflow

**Commando‑framing (officieel)**  
Een CAT‑commando bestaat uit (1) **2 letters**, (2) **vooraf vastgelegde parameters** met **vast aantal digits**, en (3) de terminator **`;`** die het einde aangeeft. citeturn3view0turn22view0 De handleiding benadrukt dat parameters exact op lengte moeten zijn; ontbrekende digits/direction‑tekens leveren foutief gedrag op (voorbeeld IF‑SHIFT). citeturn22view1  
Verder geldt: als een parameter “niet van toepassing” is, moeten de corresponderende digits worden gevuld met *een willekeurig niet‑control ASCII teken*, maar **niet** met `;` of control‑codes. citeturn22view1

**Seriële instellingen (praktijk/primair vs. de‑facto)**  
De Yaesu CAT‑manual specificeert **CAT RATE** (4800/9600/19200/38400 bps) en **CAT TOT** (10/100/1000/3000 ms) als menu‑instellingen. citeturn36view0turn39view0  
De manual is minder expliciet over stopbits/handshake; Hamlib (open‑source) kiest voor de FT‑991 backend als defaults: **8 databits, geen parity, 2 stopbits** en **hardware handshake** (RTS/CTS), met de expliciete kanttekening dat stopbits “assumed since manual makes no mention”. citeturn21view0  
In de praktijk werken veel CAT‑programma’s ook zonder RTS/CTS; als je geen RTS/CTS bedraad hebt, zet flow control uit of gebruik de USB‑Enhanced COM poort die virtueel de benodigde handshakes aanbiedt (software‑/driver‑afhankelijk).

**USB: Enhanced vs Standard COM port (officieel driver‑document)**  
Na installatie zie je twee poorten in Device Manager: Enhanced en Standard. Yaesu definieert hun rollen als:  
Enhanced: “CAT Communications (Frequency and Communication Mode Settings) and firmware updating”  
Standard: “TX Controls (PTT control, CW Keying, Digital Mode Operation)”. citeturn29view0  
Interpretatie: stuur **de CAT‑tekstcommando’s** (AB…ZI) via **Enhanced**; gebruik Standard wanneer software PTT/CW/FSK via **RTS/DTR/FSK** wil doen (dus *niet* via `TX`/`MX` commando’s).

**Algemene flow (Read → Answer)**

```mermaid
sequenceDiagram
  participant PC as PC/Software
  participant RIG as FT-991A (CAT)
  PC->>RIG: "FA;"  (read VFO-A frequency)
  RIG-->>PC: "FA145500000;" (answer, 9-digit Hz)
```

De terminator `;` is het eind‑teken; in terminaltools moet je meestal voorkomen dat er automatisch CR/LF achteraan komt (zie praktische sectie). citeturn3view0turn22view0

**Speciale flow: PS (POWER SWITCH) met ‘dummy data’ en timing**

```mermaid
sequenceDiagram
  participant PC as PC/Software
  participant RIG as FT-991A
  Note over PC,RIG: Handleiding: eerst "dummy data" sturen, dan na 1s en <2s het PS-commando.
  PC->>RIG: "......;" (dummy data; inhoud niet gespecificeerd)
  Note over PC: wacht ~1.0s
  PC->>RIG: "PS1;" (Power ON) of "PS0;" (Power OFF)
  RIG-->>PC: (optioneel) "PS1;" als status/answer bij read of echo
```

De timing‑eis is letterlijk benoemd in de Yaesu documentatie. citeturn27view1turn15view0

## Command‑index en indeling

**Set/Read/Answer/AI concept (officieel)**  
De Yaesu CAT‑manual onderscheidt: **Set** (schrijf actie), **Read** (vraag status), **Answer** (het antwoordformaat) en bij sommige commando’s **AI** (“Auto Information”) waarbij de transceiver automatisch statusupdates kan pushen. citeturn3view0

**Compacte vergelijking van commando‑types**

| Type | Betekenis | Minimale bytes | Voorbeeld ASCII | Voorbeeld HEX (ASCII bytes) |
|---|---|---:|---|---|
| Set | Zet een instelling | ≥3 | `AI1;` | `41 49 31 3B` |
| Read | Vraagt een waarde op | ≥3 | `FA;` | `46 41 3B` |
| Answer | Antwoord met waarde | variabel | `FA145500000;` | `46 41 31 34 35 35 30 30 30 30 30 3B` |

(HEX is simpelweg de ASCII‑codes van de verzonden tekens.) citeturn3view0turn22view0

**Alphabetische lijst van ondersteunde CAT‑commando’s (FT‑991A)**  
De officiële “Control Command List” in de FT‑991A CAT manual bevat **91 commando’s** met hun Set/Read/Answer/AI ondersteuning. citeturn3view0  
Je kunt ze grofweg functioneel indelen als:

| Functiegebied | Typische commando’s (niet uitputtend) |
|---|---|
| Frequentie/VFO/Mode | `FA`, `FB`, `MD`, `IF`, `OI`, `SV`, `FT` citeturn12view0turn13view1turn16view0 |
| TX/PTT/VOX | `TX`, `MX`, `VX`, `VG`, `VD`, `TS` citeturn10view1turn32view0 |
| Memory | `MC`, `MR`, `MW`, `MT`, `MA`, `AM`, `VM`, `QI`, `QR` citeturn14view1turn15view0 |
| Receiver/audio DSP | `AG`, `RG`, `NB`, `NL`, `NR`, `RA`, `PA`, `BC`, `BP`, `CO` citeturn32view0turn37view0 |
| Repeater/tones | `CT`, `CN`, `OS` citeturn11view0turn16view0 |
| Utility/status/meters | `ID`, `RI`, `RM`, `SM`, `RS`, `DT`, `DA` citeturn13view0turn31view0turn11view2 |
| Menu‑toegang | `EX` (toegang tot menu‑items incl. CAT RATE/TOT/RTS) citeturn7view0turn36view0 |

## Volledige commandoreferentie van FT‑991A

### Notatie en vaste veldlengtes

In plaats van P1/P2‑labels gebruik ik hieronder een **praktische notatie**:

- `f9` = **9 digits** frequentie in Hz (bijv. `145500000`) citeturn12view0  
- `n3` = **3 digits** (bijv. `050`)  
- `n4` = **4 digits** (bijv. `0030`)  
- `±n4` = teken `+` of `-` plus 4 digits (bijv. `+1000`) voor IF‑SHIFT/clarifier citeturn13view1turn24view1  
- Elk commando eindigt met `;` citeturn3view0turn22view0

### Complete set: ASCII/HEX voorbeelden per commando

Onderstaand staat voor **alle 91 commando’s** minimaal één **voorbeeldstring** en de bijbehorende **HEX bytes** (ASCII). Voor commando’s die een Read/Answer hebben, staat ook een Read en Reply voorbeeld. De syntaxis en parameter‑ranges zijn gebaseerd op de officiële Yaesu FT‑991A CAT‑manual (command tables). citeturn3view0turn8view0turn16view0

```text
CMD	SET_ASCII	SET_HEX	READ_ASCII	READ_HEX	REPLY_ASCII	REPLY_HEX
AB	AB;	41 42 3B				
AC	AC001;	41 43 30 30 31 3B	AC;	41 43 3B	AC001;	41 43 30 30 31 3B
AG	AG0128;	41 47 30 31 32 38 3B	AG0;	41 47 30 3B	AG0128;	41 47 30 31 32 38 3B
AI	AI1;	41 49 31 3B	AI;	41 49 3B	AI1;	41 49 31 3B
AM	AM;	41 4D 3B				
BA	BA;	42 41 3B				
BC	BC01;	42 43 30 31 3B	BC0;	42 43 30 3B	BC01;	42 43 30 31 3B
BD	BD0;	42 44 30 3B				
BI	BI1;	42 49 31 3B	BI;	42 49 3B	BI1;	42 49 31 3B
BP	BP01100;	42 50 30 31 31 30 30 3B	BP01;	42 50 30 31 3B	BP01100;	42 50 30 31 31 30 30 3B
BS	BS16;	42 53 31 36 3B				
BU	BU0;	42 55 30 3B				
BY			BY;	42 59 3B	BY10;	42 59 31 30 3B
CH	CH0;	43 48 30 3B				
CN	CN00010;	43 4E 30 30 30 31 30 3B	CN00;	43 4E 30 30 3B	CN00010;	43 4E 30 30 30 31 30 3B
CO	CO000001;	43 4F 30 30 30 30 30 31 3B	CO00;	43 4F 30 30 3B	CO000001;	43 4F 30 30 30 30 30 31 3B
CS	CS1;	43 53 31 3B	CS;	43 53 3B	CS1;	43 53 31 3B
CT	CT02;	43 54 30 32 3B	CT0;	43 54 30 3B	CT02;	43 54 30 32 3B
DA	DA000210;	44 41 30 30 30 32 31 30 3B	DA;	44 41 3B	DA000210;	44 41 30 30 30 32 31 30 3B
DN	DN;	44 4E 3B				
DT	DT020260323;	44 54 30 32 30 32 36 30 33 32 33 3B	DT0;	44 54 30 3B	DT020260323;	44 54 30 32 30 32 36 30 33 32 33 3B
ED	ED010;	45 44 30 31 30 3B				
EK	EK;	45 4B 3B				
EU	EU010;	45 55 30 31 30 3B				
EX	EX0313;	45 58 30 33 31 33 3B	EX031;	45 58 30 33 31 3B	EX0313;	45 58 30 33 31 33 3B
FA	FA145500000;	46 41 31 34 35 35 30 30 30 30 30 3B	FA;	46 41 3B	FA145500000;	46 41 31 34 35 35 30 30 30 30 30 3B
FB	FB433500000;	46 42 34 33 33 35 30 30 30 30 30 3B	FB;	46 42 3B	FB433500000;	46 42 34 33 33 35 30 30 30 30 30 3B
FS	FS1;	46 53 31 3B	FS;	46 53 3B	FS1;	46 53 31 3B
FT	FT3;	46 54 33 3B	FT;	46 54 3B	FT1;	46 54 31 3B
GT	GT04;	47 54 30 34 3B	GT0;	47 54 30 3B	GT04;	47 54 30 34 3B
ID			ID;	49 44 3B	ID0670;	49 44 30 36 37 30 3B
IF			IF;	49 46 3B	IF001145500000+000000400000;	49 46 30 30 31 31 34 35 35 30 30 30 30 30 2B 30 30 30 30 30 30 34 30 30 30 30 30 3B
IS	IS0+1000;	49 53 30 2B 31 30 30 30 3B	IS0;	49 53 30 3B	IS0+1000;	49 53 30 2B 31 30 30 30 3B
KM	KM1CQ CQ TEST;	4B 4D 31 43 51 20 43 51 20 54 45 53 54 3B	KM1;	4B 4D 31 3B	KM1CQ CQ TEST;	4B 4D 31 43 51 20 43 51 20 54 45 53 54 3B
KP	KP50;	4B 50 35 30 3B	KP;	4B 50 3B	KP50;	4B 50 35 30 3B
KR	KR1;	4B 52 31 3B	KR;	4B 52 3B	KR1;	4B 52 31 3B
KS	KS020;	4B 53 30 32 30 3B	KS;	4B 53 3B	KS020;	4B 53 30 32 30 3B
KY	KY1;	4B 59 31 3B				
LK	LK1;	4C 4B 31 3B	LK;	4C 4B 3B	LK1;	4C 4B 31 3B
LM	LM01;	4C 4D 30 31 3B	LM0;	4C 4D 30 3B	LM01;	4C 4D 30 31 3B
MA	MA;	4D 41 3B				
MC	MC001;	4D 43 30 30 31 3B	MC;	4D 43 3B	MC001;	4D 43 30 30 31 3B
MD	MD04;	4D 44 30 34 3B	MD0;	4D 44 30 3B	MD04;	4D 44 30 34 3B
MG	MG050;	4D 47 30 35 30 3B	MG;	4D 47 3B	MG050;	4D 47 30 35 30 3B
ML	ML0001;	4D 4C 30 30 30 31 3B	ML0;	4D 4C 30 3B	ML0001;	4D 4C 30 30 30 31 3B
MR			MR001;	4D 52 30 30 31 3B	MR001145500000+000000410000;	4D 52 30 30 31 31 34 35 35 30 30 30 30 30 2B 30 30 30 30 30 30 34 31 30 30 30 30 3B
MS	MS2;	4D 53 32 3B	MS;	4D 53 3B	MS2;	4D 53 32 3B
MT	MT001145500000+0000004000010MEM001TEST12;	4D 54 30 30 31 31 34 35 35 30 30 30 30 30 2B 30 30 30 30 30 30 34 30 30 30 30 31 30 4D 45 4D 30 30 31 54 45 53 54 31 32 3B	MT001;	4D 54 30 30 31 3B	MT001145500000+0000004000010MEM001TEST12;	4D 54 30 30 31 31 34 35 35 30 30 30 30 30 2B 30 30 30 30 30 30 34 30 30 30 30 31 30 4D 45 4D 30 30 31 54 45 53 54 31 32 3B
MW	MW001145500000+000000400001;	4D 57 30 30 31 31 34 35 35 30 30 30 30 30 2B 30 30 30 30 30 30 34 30 30 30 30 31 3B				
MX	MX1;	4D 58 31 3B	MX;	4D 58 3B	MX1;	4D 58 31 3B
NA	NA01;	4E 41 30 31 3B	NA0;	4E 41 30 3B	NA01;	4E 41 30 31 3B
NB	NB01;	4E 42 30 31 3B	NB0;	4E 42 30 3B	NB01;	4E 42 30 31 3B
NL	NL0005;	4E 4C 30 30 30 35 3B	NL0;	4E 4C 30 3B	NL0005;	4E 4C 30 30 30 35 3B
NR	NR01;	4E 52 30 31 3B	NR0;	4E 52 30 3B	NR01;	4E 52 30 31 3B
OI			OI;	4F 49 3B	OI001433500000+000000400000;	4F 49 30 30 31 34 33 33 35 30 30 30 30 30 2B 30 30 30 30 30 30 34 30 30 30 30 30 3B
OS	OS01;	4F 53 30 31 3B	OS0;	4F 53 30 3B	OS01;	4F 53 30 31 3B
PA	PA02;	50 41 30 32 3B	PA0;	50 41 30 3B	PA02;	50 41 30 32 3B
PB	PB01;	50 42 30 31 3B	PB0;	50 42 30 3B	PB01;	50 42 30 31 3B
PC	PC050;	50 43 30 35 30 3B	PC;	50 43 3B	PC050;	50 43 30 35 30 3B
PL	PL050;	50 4C 30 35 30 3B	PL;	50 4C 3B	PL050;	50 4C 30 35 30 3B
PR	PR12;	50 52 31 32 3B	PR1;	50 52 31 3B	PR12;	50 52 31 32 3B
PS	PS1;	50 53 31 3B	PS;	50 53 3B	PS1;	50 53 31 3B
QI	QI;	51 49 3B				
QR	QR;	51 52 3B				
QS	QS;	51 53 3B				
RA	RA01;	52 41 30 31 3B	RA0;	52 41 30 3B	RA01;	52 41 30 31 3B
RC	RC;	52 43 3B				
RD	RD0020;	52 44 30 30 32 30 3B				
RG	RG0128;	52 47 30 31 32 38 3B	RG0;	52 47 30 3B	RG0128;	52 47 30 31 32 38 3B
RI			RI0;	52 49 30 3B	RI01;	52 49 30 31 3B
RL	RL005;	52 4C 30 30 35 3B	RL0;	52 4C 30 3B	RL005;	52 4C 30 30 35 3B
RM			RM1;	52 4D 31 3B	RM1128;	52 4D 31 31 32 38 3B
RS			RS;	52 53 3B	RS0;	52 53 30 3B
RT	RT1;	52 54 31 3B	RT;	52 54 3B	RT1;	52 54 31 3B
RU	RU0020;	52 55 30 30 32 30 3B				
SC	SC1;	53 43 31 3B	SC;	53 43 3B	SC1;	53 43 31 3B
SD	SD0200;	53 44 30 32 30 30 3B	SD;	53 44 3B	SD0200;	53 44 30 32 30 30 3B
SH	SH000;	53 48 30 30 30 3B	SH0;	53 48 30 3B	SH000;	53 48 30 30 30 3B
SM			SM0;	53 4D 30 3B	SM0128;	53 4D 30 31 32 38 3B
SQ	SQ0050;	53 51 30 30 35 30 3B	SQ0;	53 51 30 3B	SQ0050;	53 51 30 30 35 30 3B
SV	SV;	53 56 3B				
TS	TS1;	54 53 31 3B	TS;	54 53 3B	TS1;	54 53 31 3B
TX	TX1;	54 58 31 3B	TX;	54 58 3B	TX1;	54 58 31 3B
UL			UL;	55 4C 3B	UL0;	55 4C 30 3B
UP	UP;	55 50 3B				
VD	VD0500;	56 44 30 35 30 30 3B	VD;	56 44 3B	VD0500;	56 44 30 35 30 30 3B
VG	VG050;	56 47 30 35 30 3B	VG;	56 47 3B	VG050;	56 47 30 35 30 3B
VM	VM;	56 4D 3B				
VX	VX1;	56 58 31 3B	VX;	56 58 3B	VX1;	56 58 31 3B
XT	XT1;	58 54 31 3B	XT;	58 54 3B	XT1;	58 54 31 3B
ZI	ZI;	5A 49 3B				
```

### Belangrijke commando‑structuren met betekenis van velden

Hieronder de “load‑bearing” commando’s die vaak nodig zijn voor eigen software (freq/mode/memory/PTT/status), met veldbetekenis en bekende eisen uit de officiële Yaesu tabellen:

**FA/FB — VFO frequentie**  
`FAf9;` en `FBf9;` zetten (Set) of antwoorden (Answer) met een **9‑digit Hz frequentie** binnen `000030000` t/m `470000000`. Read is `FA;` / `FB;`. citeturn12view0

**MD — Operating mode**  
`MD0m;` waarbij `m` een mode‑code is (LSB/USB/CW/FM/AM/RTTY/DATA/FM‑N/AM‑N/C4FM, etc.). Read `MD0;` antwoordt met `MD0m;`. citeturn13view1turn14view0

**IF — “Information” (samengestelde statusregel)**  
Read `IF;` → Answer `IFccc f9 ±n4 r x m v t 00 o;` (zonder spaties), gdje:
- `ccc` = memory channel (001–117) citeturn13view1  
- `f9` = VFO‑A frequentie  
- `±n4` = clarifier richting en offset (plus/min en 4 digits) citeturn13view1  
- `r` = RX‑clar on/off, `x` = TX‑clar on/off  
- `m` = mode‑code (zoals bij MD) citeturn13view1  
- `v` = VFO/Memory/MT/QMB/PMS/Home indicator  
- `t` = tone‑mode (CTCSS/DCS)  
- `00` = fixed  
- `o` = repeater shift (simplex/plus/minus) citeturn13view1turn16view0

**OI — “Opposite band information” (VFO‑B statusregel)**  
Read `OI;` → structure analoog aan IF maar met **VFO‑B frequentie** en bijbehorende velden. citeturn16view0turn37view0  
(Hamlib gebruikt o.a. `OI;` intern om split‑mode te bepalen, wat impliciet bevestigt dat dit een kerncommando is.) citeturn21view0

**MX en TX — PTT/CAT‑TX**  
`MXp;` zet MOX (p=0/1). `TXp;` bestuurt “CAT TX ON/OFF”; `TX;` geeft status terug. citeturn37view0turn10view1turn32view0  
Let op: als je software liever de Standard‑COM gebruikt voor PTT via RTS/DTR, dan gebruik je niet `TX`/`MX` maar hardwarelijnen; dat is precies de reden waarom Yaesu de Standard‑COM als “TX Controls” labelt. citeturn29view0

**MR/MW/MT — Memory read/write en tag**  
- `MRccc;` leest een memory channel en antwoordt met een lange `MR...;` regel (vergelijkbaar met IF). citeturn14view1  
- `MW...;` schrijft een memory channel (zonder tag). citeturn9view2  
- `MT...TAG...;` schrijft memory channel **plus “TAG characters (up to 12)”** en heeft ook een read `MTccc;` die de volledige record terugstuurt. citeturn14view2  

**OS — Repeater shift**  
`OS0p;` met `p` = 0 simplex, 1 plus, 2 minus, maar “*can be activated only with an FM mode*”. citeturn16view0turn27view2

**VD/VG/VX — VOX instellingen**  
VOX delay (`VD`) en VOX gain (`VG`) zijn instelbaar, en `VX` toggelt VOX. Cruciaal: de handleiding zegt expliciet dat `VD` een andere betekenis krijgt afhankelijk van menu‑item “142 VOX SELECT” (MIC vs DATA). citeturn10view0turn27view3

**PS — Power switch**  
`PS0;`/`PS1;` bestaat, maar de manual geeft een procedure: eerst “dummy data”, dan na 1 seconde en vóór 2 seconden het PS‑commando versturen. citeturn27view1turn15view0

**RI/RM/SM — status en meters**  
- `RIx;` geeft voor een specifieke indicator (`x`) een 0/1 terug (bijv. Hi‑SWR, TX LED, REC/PLAY, VFO‑TX/RX). citeturn31view0turn30view1  
- `RMx;` leest een meterwaarde (0–255) en antwoordt `RMxvvv;`. citeturn31view0turn30view1  
- `SM0;` leest S‑meter scale 0–255 en antwoordt `SM0vvv;`. citeturn9view3turn32view0

**NA (NARROW) — documentatie‑valkuil**  
In de command‑lijst heet het commando `NA` (NARROW). citeturn3view0  
Maar in de raster‑tabel op pagina 13 lijkt een typfout te staan waarbij in het commando‑raster “MA” wordt getoond onder NA. citeturn37view0  
Omdat `MA` als aparte functie bestaat (“Memory Channel to VFO‑A”), is dit waarschijnlijk een druk-/layoutfout; test dit commando in jouw firmware met een read en controleer of de radio antwoordt.

## Verschillen FT‑991 vs FT‑991A en implementaties in Hamlib/CHIRP

**FT‑991 vs FT‑991A verschillen in officiële CAT docs (meetbaar)**  
- `ID;` antwoord: FT‑991 geeft `ID0570;` citeturn24view1 terwijl FT‑991A `ID0670;` geeft. citeturn13view0  
- `IS` (IF‑SHIFT) bereik: FT‑991 documenteert **–1000…+1000 Hz** citeturn24view1, FT‑991A **–1200…+1200 Hz**. citeturn13view1  
- `EX` menu‑range: FT‑991 **001–151** citeturn25view0, FT‑991A **001–153** (o.a. extra WIRES/DG‑ID items). citeturn7view0turn27view3

**Hamlib (open‑source) mapping**  
Hamlib heeft een FT‑991 backend die seriële defaults zet op o.a. 8N2 en hardware handshake (assumptie), en gebruikt “newcat” infrastructuur. citeturn21view0  
In `newcat.c` staan rig‑ID’s expliciet als 570 (FT‑991) en 670 (FT‑991A) en de terminator is `;`. citeturn34view0turn35view0  
Dit is nuttig om te begrijpen waarom `ID;` als modeldetectie in veel software gebruikt wordt.

**CHIRP en vendor‑specifieke / undocumented commando’s**  
CHIRP heeft historisch issues rond FT‑991(A) memory tags en memory‑structuur; in issue #2531 is een RT Systems “handshake” gelogd met commando’s zoals `SPID`, `SPR`, `SPW`, met een `A;` ACK na writes. citeturn42view0turn41view0  
Belangrijk: deze `SP*` commando’s staan **niet** in de officiële Yaesu CAT command list voor FT‑991A. citeturn3view0turn41view0  
Een externe analysetekst wijst er bovendien op dat MT/MW (officieel gedocumenteerd) in de praktijk soms niet “round‑trip” reproduceerbaar zijn op basis van het antwoordformaat, en dat repeater shifts buiten de band‑definitie lastig kunnen zijn. citeturn41view0turn42view0  
Conclusie: blijf voor eigen tooling primair bij de **officiële AB…ZI commandoset**; als je toch met `SP*` experimenteert, doe dat alleen met volledige backups en op laag risico.

## Praktische voorbeelden: terminaltools en Python

**Windows: RealTerm (aanrader boven PuTTY voor raw ASCII)**  
1. Kies **Enhanced COM Port** uit Device Manager (bijv. COM8). citeturn29view0  
2. Stel baud in conform radio‑menu (CAT RATE). citeturn36view0turn39view0  
3. In “Send” stuur je exact de string (zonder CR/LF), bv. `ID;` of `FA;`.  
4. Verwacht `ID0670;` als antwoord bij FT‑991A. citeturn13view0  

**Linux/macOS: minicom/screen**  
- `screen /dev/ttyUSB0 38400` werkt, maar let op dat sommige terminals Enter als CR sturen. Je wilt écht alleen `;` als terminator. citeturn3view0turn22view0  
- `minicom` is vaak makkelijker omdat je lokale echo/line endings kunt uitschakelen.

**Python (pyserial) — minimale send/read helper**  
Onderstaand voorbeeld stuurt een commando, leest tot `;` terug, en toont zowel ASCII als hex.

```python
import serial
from typing import Optional

def to_hex(b: bytes) -> str:
    return " ".join(f"{x:02X}" for x in b)

def cat_query(port: str, baud: int, cmd: str, timeout: float = 1.0) -> Optional[str]:
    if not cmd.endswith(";"):
        raise ValueError("CAT commando moet eindigen op ';'")
    with serial.Serial(
        port=port,
        baudrate=baud,
        bytesize=serial.EIGHTBITS,
        parity=serial.PARITY_NONE,
        stopbits=serial.STOPBITS_TWO,   # vaak OK; sommige setups werken met 1
        timeout=timeout,
        rtscts=False                    # zet True als je expliciet RTS/CTS gebruikt
    ) as ser:
        ser.reset_input_buffer()
        ser.write(cmd.encode("ascii"))
        raw = ser.read_until(b";")
        if not raw:
            return None
        return raw.decode("ascii", errors="replace")

if __name__ == "__main__":
    # Pas COM-poort en baud aan (CAT RATE)
    ans = cat_query("COM8", 38400, "ID;")
    print("ASCII:", ans)
    if ans is not None:
        print("HEX:", to_hex(ans.encode("ascii")))
```

Opmerking: De officiële docs beschrijven CAT RATE/TOT, maar niet altijd stopbits/handshake; Hamlib gebruikt 2 stopbits als aanname voor FT‑991. citeturn21view0turn36view0

**Voorbeeld: read‑modify‑write cycle voor een memory tag (MT)**  
1. Selecteer channel: `MT001;` (read) → ontvang volledige record inclusief tag. citeturn14view2  
2. Pas in je software alleen de tag‑string (max. 12 ASCII chars) aan. citeturn14view2  
3. Schrijf terug met een volledige `MT...;` record (zoals in de TSV hierboven).  
4. Lees opnieuw `MT001;` ter verificatie.

## Troubleshooting en test checklist

**Geen antwoord van radio**
- Verkeerde COM poort: gebruik bij USB de **Enhanced COM Port** voor CAT. citeturn29view0  
- Baud mismatch: controleer radio‑menu **CAT RATE** en match in software. citeturn36view0turn39view0  
- Terminator ontbreekt of extra CR/LF: de terminator is **`;`**; extra line endings kunnen problemen geven. citeturn3view0turn22view0  

**Garbled tekst / vreemde tekens**
- Verkeerde bitrate of framing. Hamlib hanteert 8N2 als default-aanname voor FT‑991; probeer 2 stopbits als 1 niet werkt. citeturn21view0  
- Controleer dat je ASCII stuurt (geen UTF‑8 multibyte, geen “smart quotes”).

**Commando werkt alleen in bepaalde mode**
- `OS` (repeater shift) werkt alleen in **FM**. citeturn16view0turn27view2  
- `VD` (VOX delay) hangt af van menu “142 VOX SELECT” (MIC/DATA). citeturn10view0turn27view3  

**PS (power) faalt**
- Volg de vereiste timing: dummy data, dan na 1s en vóór 2s het `PS`‑commando. citeturn27view1turn15view0  

**Memory/tags inconsistent**
- Als MT/MW round‑trip niet lukt: er zijn meldingen dat antwoorden niet altijd 1‑op‑1 herbruikbaar zijn, en dat sommige tools (RT Systems) daarom `SPID/SPR/SPW` gebruiken. citeturn41view0turn42view0  
- Werk conservatief: schrijf alleen wat nodig is, verifieer met read‑back, en maak backups.

**Korte test checklist**
1. `ID;` → verwacht `ID0670;` (FT‑991A). citeturn13view0  
2. `FA;` → lees huidige VFO‑A; schrijf terug met `FAf9;`. citeturn12view0  
3. `MD0;` → lees mode; zet naar FM met `MD04;`. citeturn14view0turn13view1  
4. `MX1;` → MOX aan, dan `MX0;` uit. citeturn37view0  
5. `MR001;` of `MT001;` → lees memory record, verifieer antwoordlengte en terminator. citeturn14view1turn14view2  

```text
Primaire bronnen (URLs alleen in codeblock i.v.m. opmaakregels):
- https://www.yaesu.com/FileLibraryF/4CB893D7-1018-01AF-FA97E9E9AD48B50C/FT-991A_CAT_OM_ENG_1711-D.pdf
- https://www.yaesu.com/Files/4CB893D7-1018-01AF-FA97E9E9AD48B50C/FT-991_CAT_OM_ENG_1612-D0.pdf
- https://www.yaesu.com/Files/BB2B47AE-1018-01AF-FAE48FDCB1919193/USB_Driver_Installation_Manual_ENG_2202-D.pdf
Secundair / implementaties:
- https://raw.githubusercontent.com/Hamlib/Hamlib/master/rigs/yaesu/ft991.c
- https://raw.githubusercontent.com/Hamlib/Hamlib/master/rigs/yaesu/newcat.c
- https://chirpmyradio.com/issues/2531
- https://raw.githubusercontent.com/j0ju/ft991a-interoperability-tools/main/CAT.md
```

