# Yaesu FT‑991A: alle 153 EX‑menuparameters uitlezen en wijzigen via CAT

## Executive summary

De FT‑991A ondersteunt het uitlezen en instellen van **alle “Menu Mode” items 001 t/m 153** via het **CAT “EX” commando**: *read* met `EXnnn;` en *set* met `EXnnn<P2…>;`, waarbij `nnn` het **3‑cijferige menunummer** is en `P2` de **gecodeerde waarde** met een menu‑afhankelijke veldlengte (“P2 Digits”). citeturn51view0turn52view0  
De **Yaesu FT‑991A CAT Operation Reference Manual** is de primaire bron voor de **EX‑commando‑syntaxis, P2‑codering en veldlengtes** per menu‑item (001–153). citeturn51view0turn51view1turn51view2  
De **FT‑991A Operating Manual** (Menu Mode tabel) is een primaire bron voor “**Available Settings**” en “**Default Value**” van alle 153 menu‑items; in deze rapportage zijn die defaults samengevoegd met de Yaesu‑CAT codering om een complete “EX‑menu via CAT” referentie te maken. citeturn49view4turn50view4turn47view4  
In de praktijk is de grootste valkuil dat je via EX ook **CAT‑snelheid/timeout/RTS** kunt wijzigen (menu 031–033 en 032 CAT TOT), waardoor je (tijdelijk) je eigen verbinding kunt “wegconfigureren”. De Operating Manual toont de beschikbare waardes en defaults; de CAT manual toont de bijbehorende P2‑codes. citeturn49view4turn51view0turn51view1  

## Bronnen, scope en aannames

Deze referentie combineert twee primaire Yaesu‑bronnen:  
De **CAT Operation Reference Manual** definieert de CAT‑commandostructuur (terminator `;`, parameters met vaste lengte) en specificeert het **EX MENU** commando inclusief de volledige tabel “P1 Function / P2 / Digits” voor menu 001–153. citeturn52view0turn51view0turn51view1turn51view2  
De **Operating Manual** publiceert een **Menu Mode overzichtstabel** met “Menu Function / Available Settings / Default Value” voor menu 001–153, over meerdere pagina’s. citeturn49view4turn50view4turn47view4  

Aannames: je noemde geen specifieke OS/firmware/driver‑versies, dus dit document gaat uit van **geen specifieke constraints**. Waar instellingen firmware‑ of regio‑afhankelijk zijn, staat dat expliciet vermeld (bijv. EU‑voetnoot in de menu‑tabel). citeturn49view4  

## CAT‑transport en commando‑opbouw

### Fysieke verbindingen en poortkeuze

Yaesu beschrijft twee hoofdwegen:  
Via de **GPS/CAT (RS‑232C) aansluiting** op de achterzijde; bij RS‑232 gebruik moet je menu **028 GPS/232C SELECT** op “RS232C” zetten. citeturn52view0turn49view4turn51view0  
Via de **USB‑kabel**: de FT‑991A heeft een **USB‑to‑Dual‑UART bridge** en vereist een **USB‑driver** voor remote control vanaf een PC. citeturn52view0  

In Windows presenteert de Yaesu/CP210x‑driver doorgaans **twee COM‑poorten (Enhanced en Standard)**. In veel praktijk‑documentatie wordt de **Enhanced COM‑port** gebruikt voor CAT‑control en de **Standard COM‑port** eerder voor CW/RTTY‑keying/FSK‑achtige functies; bevestig dit op jouw systeem in Device Manager. citeturn31search12  

### Seriële framing en snelheidsparameters

De Yaesu CAT manual definieert expliciet dat CAT‑commando’s bestaan uit **2 letters + parameters + terminator `;`**. citeturn52view0  
De baudrate‑keuze (en CAT timeout) wordt in de Menu Mode tabel weergegeven als:  
**031 CAT RATE**: 4800/9600/19200/38400 bps (default 4800 bps) en **032 CAT TOT**: 10/100/1000/3000 ms (default 10 ms). citeturn49view4  
Voor RS‑232 bestaan analoge waarden: **029 232C RATE** en **030 232C TOT**. citeturn49view4turn51view0  

De Yaesu CAT manual in de geciteerde passages specificeert niet alle framing‑details (databits/pariteit/stopbits). In open‑source implementaties (Hamlib) voor de FT‑991/FT‑991A wordt typisch **8 databits, geen pariteit, 2 stopbits, geen handshake** aangehouden (8N2 zonder flow control). Gebruik dit als compatibel startpunt wanneer je tooling dat vereist. citeturn0search2  

### Terminator‑regel en “vaste veldlengtes”

Yaesu stelt: de CAT terminator is **een puntkomma `;`** en de parameterlengtes zijn vooraf bepaald; onjuiste lengte (te weinig/te veel digits) is een klassieke foutbron. citeturn52view0turn51view0  

## EX‑menu via CAT: lees‑ en schrijfpatroon

### EX‑commando syntaxis

De CAT Operation Reference Manual specificeert:

- **Set**: `EX P1P1P1 P2P2…;` waarbij `P1` het menu‑nummer is (001–153) en `P2` de parameter volgens de tabel en “P2 Digits”. citeturn51view0  
- **Read**: `EX P1P1P1;`  
- **Answer**: `EX P1P1P1 P2P2…;` citeturn51view0  

Belangrijk: “P2 Digits” varieert per menu‑item (bijv. 1, 2, 3, 4, 5 of 8 digits). citeturn51view0turn51view1turn51view2  

### Protocolflow (mermaid)

```mermaid
sequenceDiagram
  participant PC as PC/Controller
  participant RIG as FT-991A

  PC->>RIG: EXnnn;  (Read)
  RIG-->>PC: EXnnnP2...; (Answer, P2 lengte = "P2 Digits")

  Note over PC,RIG: "nnn" is menu 001..153 (3 digits), terminator is ';'
```

```mermaid
sequenceDiagram
  participant PC as PC/Controller
  participant RIG as FT-991A

  PC->>RIG: EXnnn;  (Read current)
  RIG-->>PC: EXnnnP2...;

  PC->>RIG: EXnnnNEW...; (Set new value)
  Note over PC,RIG: Let op: NEW veldlengte exact volgens "P2 Digits"
  PC->>RIG: EXnnn;  (Verify)
  RIG-->>PC: EXnnnNEW...;
```

### Klein commandotabelletje (read vs write) met ASCII en hex‑bytes

| Actie | ASCII voorbeeld | Hex‑bytes (ASCII) | Betekenis |
|---|---|---|---|
| Read menu 031 (CAT RATE) | `EX031;` | `45 58 30 33 31 3B` | Vraag huidige CAT RATE op citeturn51view0turn51view1 |
| Set menu 031 naar 38400 bps (code 3) | `EX0313;` | `45 58 30 33 31 33 3B` | Zet CAT RATE naar 38400 bps (P2=3) citeturn51view1turn49view4 |
| Set menu 143 VOX GAIN naar 50 (3 digits) | `EX143050;` | `45 58 31 34 33 30 35 30 3B` | Zet VOX GAIN op 50 (000–100) citeturn51view2turn47view4 |

Toelichting bij de voorbeelden: menu 031 is een enumeratie (P2 Digits = 1), menu 143 is numeriek (P2 Digits = 3 met leading zeros). citeturn51view1turn51view2  

### Sjabloon voor “read‑modify‑write” dat op elk menu‑item past

1. **Lees**: stuur `EX{menu:03d};` en wacht op antwoord `EX{menu:03d}{value};`. citeturn51view0  
2. **Parseer**: valideer prefix `EX` + exact 3 digits menu; lees daarna alles tot `;` als P2‑payload. (De CAT manual benadrukt vaste veldlengtes en `;` als terminator.) citeturn52view0turn51view0  
3. **Valideer tegen P2‑regels**: gebruik de EX‑tabel om te bepalen:
   - aantal digits (P2 Digits),
   - of het een **enumeratie** is (0/1/2…) of een **numeriek bereik** met stappen,
   - of er een **teken** (`+`/`-`) in het veld zit (bij sommige offset‑achtige menu’s). citeturn51view0turn51view1turn51view2  
4. **Schrijf**: stuur `EX{menu:03d}{new_p2};` met exact juiste veldlengte. citeturn51view0  
5. **Verifieer**: lees terug met `EX{menu:03d};`. citeturn51view0  

Praktijkwaarschuwing: als je **CAT RATE/TOT/RTS** (menu 031–033/032) wijzigt, kan je PC‑zijde niet meer matchen; plan zo’n wijziging alsof het een “link renegotiation” is. De beschikbare waarden en defaults staan in de Operating Manual; de P2‑codes staan in de CAT manual. citeturn49view4turn51view1  

## Volledige EX‑menu catalogus met CAT‑mapping

### Autoritatieve basis

De CAT manual levert de **complete EX‑tabel** (001–153) inclusief P2‑codering en P2‑digitlengte. citeturn51view0turn51view1turn51view2  
De Operating Manual levert voor dezelfde 001–153 de **Available Settings** en **Default Value** (in drie delen: 001–052, 053–103, 104–153). citeturn49view4turn50view4turn47view4  

### Machine‑readable tabel (CSV)

Onderstaande CSV kun je kopiëren naar `ft991a_ex_menu.csv`.  
Velden: `menu_number,name,op_available_settings,op_default,cat_p2_digits,cat_p2_encoding,cat_read_cmd,cat_set_cmd_template,source_ref`

**Bronverwijzing in `source_ref` is tekstueel** (i.v.m. weergave/citaties); de primaire bronnen zijn de Yaesu CAT manual (EX‑tabel) en Yaesu Operating Manual (Menu Mode tabel). citeturn51view0turn51view1turn51view2turn49view4turn50view4turn47view4  

```csv
menu_number,name,op_available_settings,op_default,cat_p2_digits,cat_p2_encoding,cat_read_cmd,cat_set_cmd_template,source_ref
001,AGC FAST DELAY,"20-4000 (20msec/step)","300msec",4,"0020-4000 msec (20msec/step)",EX001;,EX001{P2};,"Yaesu Operating Manual Menu Mode (p125); Yaesu CAT OM EX table (p7)"
002,AGC MID DELAY,"20-4000 (20msec/step)","700msec",4,"0020-4000 msec (20msec/step)",EX002;,EX002{P2};,"Operating Manual p125; CAT OM p7"
003,AGC SLOW DELAY,"20-4000 (20msec/step)","3000msec",4,"0020-4000 msec (20msec/step)",EX003;,EX003{P2};,"Operating Manual p125; CAT OM p7"
004,HOME FUNCTION,"SCOPE/FUNCTION","SCOPE",1,"0:SCOPE 1:FUNCTION",EX004;,EX004{P2};,"Operating Manual p125; CAT OM p7"
005,MY CALL INDICATION,"OFF-5sec","1sec",1,"0-5 sec",EX005;,EX005{P2};,"Operating Manual p125; CAT OM p7"
006,DISPLAY COLOR,"BLUE/GRAY/GREEN/ORANGE/PURPLE/RED/SKY BLUE","BLUE",1,"0:BLUE 1:GRAY 2:GREEN 3:ORANGE 4:PURPLE 5:RED 6:SKY BLUE",EX006;,EX006{P2};,"Operating Manual p125; CAT OM p7"
007,DIMMER LED,"1/2","2",1,"0:1 1:2",EX007;,EX007{P2};,"Operating Manual p125; CAT OM p7"
008,DIMMER TFT,"0-15","8",2,"00-15",EX008;,EX008{P2};,"Operating Manual p125; CAT OM p7"
009,BAR MTR PEAK HOLD,"OFF/0.5/1.0/2.0 (sec)","OFF",1,"0:OFF 1:0.5s 2:1.0s 3:2.0s",EX009;,EX009{P2};,"Operating Manual p125; CAT OM p7"
010,DVS RX OUT LEVEL,"0-100","50",3,"000-100",EX010;,EX010{P2};,"Operating Manual p125; CAT OM p7"
011,DVS TX OUT LEVEL,"0-100","50",3,"000-100",EX011;,EX011{P2};,"Operating Manual p125; CAT OM p7"
012,KEYER TYPE,"OFF/BUG/ELEKEY-A/ELEKEY-B/ELEKEY-Y/ACS","ELEKEY-B",1,"0:OFF 1:BUG 2:ELEKEY-A 3:ELEKEY-B 4:ELEKEY-Y 5:ACS",EX012;,EX012{P2};,"Operating Manual p125; CAT OM p7"
013,KEYER DOT/DASH,"NOR/REV","NOR",1,"0:NORMAL 1:REVERSE",EX013;,EX013{P2};,"Operating Manual p125; CAT OM p7"
014,CW WEIGHT,"2.5-4.5","3.0",2,"25-45 (representeert 2.5-4.5)",EX014;,EX014{P2};,"Operating Manual p125; CAT OM p7"
015,BEACON INTERVAL,"OFF/1-240sec/270-690sec","OFF",3,"000:OFF 001-690 sec",EX015;,EX015{P2};,"Operating Manual p125; CAT OM p7"
016,NUMBER STYLE,"1290/AUNO/AUNT/A2NO/A2NT/12NO/12NT","1290",1,"0:1290 1:AUNO 2:AUNT 3:A2NO 4:A2NT 5:12NO 6:12NT",EX016;,EX016{P2};,"Operating Manual p125; CAT OM p7"
017,CONTEST NUMBER,"0-9999","1",4,"0000-9999",EX017;,EX017{P2};,"Operating Manual p125; CAT OM p7"
018,CW MEMORY 1,"TEXT/MESSAGE","TEXT",1,"0:TEXT 1:MESSAGE",EX018;,EX018{P2};,"Operating Manual p125; CAT OM p7"
019,CW MEMORY 2,"TEXT/MESSAGE","TEXT",1,"0:TEXT 1:MESSAGE",EX019;,EX019{P2};,"Operating Manual p125; CAT OM p7"
020,CW MEMORY 3,"TEXT/MESSAGE","TEXT",1,"0:TEXT 1:MESSAGE",EX020;,EX020{P2};,"Operating Manual p125; CAT OM p7"
021,CW MEMORY 4,"TEXT/MESSAGE","TEXT",1,"0:TEXT 1:MESSAGE",EX021;,EX021{P2};,"Operating Manual p125; CAT OM p7"
022,CW MEMORY 5,"TEXT/MESSAGE","TEXT",1,"0:TEXT 1:MESSAGE",EX022;,EX022{P2};,"Operating Manual p125; CAT OM p7"
023,NB WIDTH,"1/3/10msec","3msec",1,"0:1ms 1:3ms 2:10ms",EX023;,EX023{P2};,"Operating Manual p125; CAT OM p7"
024,NB REJECTION,"10/30/50dB","30dB",1,"0:10dB 1:30dB 2:50dB",EX024;,EX024{P2};,"Operating Manual p125; CAT OM p7"
025,NB LEVEL,"0-10","5",2,"00-10",EX025;,EX025{P2};,"Operating Manual p125; CAT OM p7"
026,BEEP LEVEL,"0-100","50",3,"000-100",EX026;,EX026{P2};,"Operating Manual p125; CAT OM p7"
027,TIME ZONE,"-12:00 - 0:00 - +14:00","0:00",5,"UTC -12:00 .. +14:00",EX027;,EX027{P2};,"Operating Manual p125; CAT OM p7"
028,GPS/232C SELECT,"GPS1/GPS2/RS232C","GPS1",1,"0:GPS1 1:GPS2 3:RS232C",EX028;,EX028{P2};,"Operating Manual p125; CAT OM p7"
029,232C RATE,"4800/9600/19200/38400 (bps)","4800bps",1,"0:4800 1:9600 2:19200 3:38400",EX029;,EX029{P2};,"Operating Manual p125; CAT OM p7"
030,232C TOT,"10/100/1000/3000 (msec)","10msec",1,"0:10ms 1:100ms 2:1000ms 3:3000ms",EX030;,EX030{P2};,"Operating Manual p125; CAT OM p7"
031,CAT RATE,"4800/9600/19200/38400 (bps)","4800bps",1,"0:4800 1:9600 2:19200 3:38400",EX031;,EX031{P2};,"Operating Manual p125; CAT OM p7"
032,CAT TOT,"10/100/1000/3000 (msec)","10msec",1,"0:10ms 1:100ms 2:1000ms 3:3000ms",EX032;,EX032{P2};,"Operating Manual p125; CAT OM p7"
033,CAT RTS,"ENABLE/DISABLE","ENABLE",1,"0:DISABLE 1:ENABLE",EX033;,EX033{P2};,"Operating Manual p125; CAT OM p7"
034,MEM GROUP,"ENABLE/DISABLE","DISABLE",1,"0:DISABLE 1:ENABLE",EX034;,EX034{P2};,"Operating Manual p125; CAT OM p7"
035,QUICK SPLIT FREQ,"-20 - 20kHz","5kHz",3,"-20..+20 kHz (P2=-20..+20)",EX035;,EX035{P2};,"Operating Manual p125; CAT OM p7"
036,TX TOT,"OFF/1-30 (min)","OFF",2,"00:OFF 01-30 min",EX036;,EX036{P2};,"Operating Manual p125; CAT OM p7"
037,MIC SCAN,"ENABLE/DISABLE","ENABLE",1,"0:DISABLE 1:ENABLE",EX037;,EX037{P2};,"Operating Manual p125; CAT OM p7"
038,MIC SCAN RESUME,"PAUSE/TIME","TIME",1,"0:PAUSE 1:TIME",EX038;,EX038{P2};,"Operating Manual p125; CAT OM p7"
039,REF FREQ ADJ,"-25 - 0 - 25","0",3,"-25..+25 (P2=-25..+25)",EX039;,EX039{P2};,"Operating Manual p125; CAT OM p7"
040,CLAR MODE SELECT,"RX/TX/TRX","RX",1,"0:RX 1:TX 2:TRX",EX040;,EX040{P2};,"Operating Manual p125; CAT OM p7-8"
041,AM LCUT FREQ,"OFF/100Hz-1000Hz (50Hz/step)","OFF",2,"00:OFF 01:100Hz .. 19:1000Hz",EX041;,EX041{P2};,"Operating Manual p125; CAT OM p7-8"
042,AM LCUT SLOPE,"6dB/oct / 18dB/oct","6dB/oct",1,"0:6dB/oct 1:18dB/oct",EX042;,EX042{P2};,"Operating Manual p125; CAT OM p7-8"
043,AM HCUT FREQ,"700Hz-4000Hz (50Hz/step)/OFF","OFF",2,"00:OFF 01:700Hz .. 67:4000Hz",EX043;,EX043{P2};,"Operating Manual p125; CAT OM p8"
044,AM HCUT SLOPE,"6dB/oct / 18dB/oct","6dB/oct",1,"0:6dB/oct 1:18dB/oct",EX044;,EX044{P2};,"Operating Manual p125; CAT OM p8"
045,AM MIC SELECT,"MIC/REAR","MIC",1,"0:MIC 1:REAR",EX045;,EX045{P2};,"Operating Manual p125; CAT OM p8"
046,AM OUT LEVEL,"0-100","50",3,"000-100",EX046;,EX046{P2};,"Operating Manual p125; CAT OM p8"
047,AM PTT SELECT,"DAKY/RTS/DTR","DAKY",1,"0:DAKY 1:RTS 2:DTR",EX047;,EX047{P2};,"Operating Manual p125; CAT OM p8"
048,AM PORT SELECT,"DATA/USB","DATA",1,"0:DATA 1:USB",EX048;,EX048{P2};,"Operating Manual p125; CAT OM p8"
049,AM DATA GAIN,"0-100","50",3,"000-100",EX049;,EX049{P2};,"Operating Manual p125; CAT OM p8"
050,CW LCUT FREQ,"OFF/100Hz-1000Hz (50Hz/step)","250Hz",2,"00:OFF 01:100Hz .. 19:1000Hz",EX050;,EX050{P2};,"Operating Manual p125; CAT OM p8"
051,CW LCUT SLOPE,"6dB/oct / 18dB/oct","18dB/oct",1,"0:6dB/oct 1:18dB/oct",EX051;,EX051{P2};,"Operating Manual p125; CAT OM p8"
052,CW HCUT FREQ,"700Hz-4000Hz (50Hz/step)/OFF","1200Hz",2,"00:OFF 01:700Hz .. 67:4000Hz",EX052;,EX052{P2};,"Operating Manual p125 (EU note); CAT OM p8"
053,CW HCUT SLOPE,"6dB/oct / 18dB/oct","18dB/oct",1,"0:6dB/oct 1:18dB/oct",EX053;,EX053{P2};,"Operating Manual p126; CAT OM p8"
054,CW OUT LEVEL,"0-100","50",3,"000-100",EX054;,EX054{P2};,"Operating Manual p126; CAT OM p8"
055,CW AUTO MODE,"OFF/50M/ON","OFF",1,"0:OFF 1:50MHz 2:ON",EX055;,EX055{P2};,"Operating Manual p126; CAT OM p8"
056,CW BK-IN TYPE,"SEMI/FULL","SEMI",1,"0:SEMI 1:FULL",EX056;,EX056{P2};,"Operating Manual p126; CAT OM p8"
057,CW BK-IN DELAY,"30-3000 (msec)","200msec",4,"0030-3000 (10msec/step)",EX057;,EX057{P2};,"Operating Manual p126; CAT OM p8"
058,CW WAVE SHAPE,"2/4 (msec)","4msec",1,"0:1ms 1:2ms 2:4ms 3:6ms",EX058;,EX058{P2};,"Operating Manual p126; CAT OM p8"
059,CW FREQ DISPLAY,"DIRECT FREQ/PITCH OFFSET","PITCH OFFSET",1,"0:DIRECT FREQ 1:PITCH OFFSET",EX059;,EX059{P2};,"Operating Manual p126; CAT OM p8"
060,PC KEYING,"OFF/DAKY/RTS/DTR","OFF",1,"0:OFF 1:DAKY 2:RTS 3:DTR",EX060;,EX060{P2};,"Operating Manual p126; CAT OM p8"
061,QSK DELAY TIME,"15/20/25/30 (msec)","15msec",1,"0:15ms 1:20ms 2:25ms 3:30ms",EX061;,EX061{P2};,"Operating Manual p126; CAT OM p8"
062,DATA MODE,"PSK/OTHERS","PSK",1,"0:PSK 1:OTHER",EX062;,EX062{P2};,"Operating Manual p126; CAT OM p8"
063,PSK TONE,"1000/1500/2000 (Hz)","1000Hz",1,"0:1000Hz 1:1500Hz 2:2000Hz",EX063;,EX063{P2};,"Operating Manual p126; CAT OM p8"
064,OTHER DISP (SSB),"-3000 - 0 - 3000 (10Hz/step)","0Hz",5,"-3000..+3000 (10Hz steps)",EX064;,EX064{P2};,"Operating Manual p126; CAT OM p8"
065,OTHER SHIFT (SSB),"-3000 - 0 - 3000 (10Hz/step)","0Hz",5,"-3000..+3000 (10Hz steps)",EX065;,EX065{P2};,"Operating Manual p126; CAT OM p8"
066,DATA LCUT FREQ,"OFF/100-1000 (50Hz/step)","300Hz",2,"00:OFF 01:100Hz .. 19:1000Hz",EX066;,EX066{P2};,"Operating Manual p126; CAT OM p8"
067,DATA LCUT SLOPE,"6dB/oct / 18dB/oct","18dB/oct",1,"0:6dB/oct 1:18dB/oct",EX067;,EX067{P2};,"Operating Manual p126; CAT OM p8"
068,DATA HCUT FREQ,"700Hz-4000Hz (50Hz/step)/OFF","3000Hz",2,"00:OFF 01:700Hz .. 67:4000Hz",EX068;,EX068{P2};,"Operating Manual p126; CAT OM p8"
069,DATA HCUT SLOPE,"6dB/oct / 18dB/oct","18dB/oct",1,"0:6dB/oct 1:18dB/oct",EX069;,EX069{P2};,"Operating Manual p126; CAT OM p8"
070,DATA IN SELECT,"REAR/MIC","REAR",1,"0:MIC 1:REAR",EX070;,EX070{P2};,"Operating Manual p126; CAT OM p8"
071,DATA PTT SELECT,"DAKY/RTS/DTR","DAKY",1,"0:DAKY 1:RTS 2:DTR",EX071;,EX071{P2};,"Operating Manual p126; CAT OM p8"
072,DATA PORT SELECT,"DATA/USB","DATA",1,"1:DATA 2:USB (Yaesu-code)",EX072;,EX072{P2};,"Operating Manual p126; CAT OM p8"
073,DATA OUT LEVEL,"0-100","50",3,"000-100",EX073;,EX073{P2};,"Operating Manual p126; CAT OM p8"
074,FM MIC SELECT,"MIC/REAR","MIC",1,"0:MIC 1:REAR",EX074;,EX074{P2};,"Operating Manual p126; CAT OM p8"
075,FM OUT LEVEL,"0-100","50",3,"000-100",EX075;,EX075{P2};,"Operating Manual p126; CAT OM p8"
076,FM PKT PTT SELECT,"DAKY/RTS/DTR","DAKY",1,"0:DAKY 1:RTS 2:DTR",EX076;,EX076{P2};,"Operating Manual p126; CAT OM p8"
077,FM PKT PORT SELECT,"DATA/USB","DATA",1,"1:DATA 2:USB (Yaesu-code)",EX077;,EX077{P2};,"Operating Manual p126; CAT OM p8"
078,FM PKT TX GAIN,"0-100","50",3,"000-100",EX078;,EX078{P2};,"Operating Manual p126; CAT OM p8"
079,FM PKT MODE,"1200/9600","1200",1,"0:1200 1:9600",EX079;,EX079{P2};,"Operating Manual p126; CAT OM p8"
080,RPT SHIFT 28MHz,"0-1000kHz (10kHz/step)","100kHz",4,"0000-1000 (10kHz/step)",EX080;,EX080{P2};,"Operating Manual p126; CAT OM p8"
081,RPT SHIFT 50MHz,"0-4000kHz (10kHz/step)","1000kHz",4,"0000-4000 (10kHz/step)",EX081;,EX081{P2};,"Operating Manual p126; CAT OM p8"
082,RPT SHIFT 144MHz,"0-4000kHz (10kHz/step)","600kHz",4,"0000-4000 (10kHz/step)",EX082;,EX082{P2};,"Operating Manual p126; CAT OM p8"
083,RPT SHIFT 430MHz,"0-10000kHz (10kHz/step)","5000kHz",5,"00000-10000 (10kHz/step)",EX083;,EX083{P2};,"Operating Manual p126; CAT OM p8"
084,ARS 144MHz,"OFF/ON","ON",1,"0:OFF 1:ON",EX084;,EX084{P2};,"Operating Manual p126; CAT OM p8"
085,ARS 430MHz,"OFF/ON","ON",1,"0:OFF 1:ON",EX085;,EX085{P2};,"Operating Manual p126; CAT OM p8"
086,DCS POLARITY,"Tn-Rn/Tn-Riv/Tiv-Rn/Tiv-Riv","Tn-Rn",1,"0:Tn-Rn 1:Tn-Riv 2:Tiv-Rn 3:Tiv-Riv",EX086;,EX086{P2};,"Operating Manual p126; CAT OM p8"
087,RADIO ID,"(read-only)","*****",0,"(geen P2; Yaesu tabel markeert als n.v.t.)",EX087;,EX087{P2};,"Operating Manual p126; CAT OM p8"
088,GM DISPLY,"DISTANCE/STRENGTH","DISTANCE",1,"0:DISTANCE 1:STRENGTH",EX088;,EX088{P2};,"Operating Manual p126; CAT OM p8"
089,DISTANCE,"km/mile","mile",1,"0:km 1:mile",EX089;,EX089{P2};,"Operating Manual p126; CAT OM p8"
090,AMS TX MODE,"AUTO/MANUAL/DN/VW/ANALOG","AUTO",1,"0:AUTO 1:MANUAL 2:DN 3:VW 4:ANALOG",EX090;,EX090{P2};,"Operating Manual p126; CAT OM p8"
091,STANDBY BEEP,"ON/OFF","ON",1,"0:OFF 1:ON",EX091;,EX091{P2};,"Operating Manual p126; CAT OM p8"
092,RTTY LCUT FREQ,"OFF/100Hz-1000Hz (50Hz/step)","300Hz",2,"00:OFF 01:100Hz .. 19:1000Hz",EX092;,EX092{P2};,"Operating Manual p126; CAT OM p8"
093,RTTY LCUT SLOPE,"6dB/oct / 18dB/oct","18dB/oct",1,"0:6dB/oct 1:18dB/oct",EX093;,EX093{P2};,"Operating Manual p126; CAT OM p8"
094,RTTY HCUT FREQ,"700Hz-4000Hz (50Hz/step)/OFF","3000Hz",2,"00:OFF 01:700Hz .. 67:4000Hz",EX094;,EX094{P2};,"Operating Manual p126; CAT OM p8"
095,RTTY HCUT SLOPE,"6dB/oct / 18dB/oct","18dB/oct",1,"0:6dB/oct 1:18dB/oct",EX095;,EX095{P2};,"Operating Manual p126; CAT OM p8"
096,RTTY SHIFT PORT,"SHIFT/DTR/RTS","SHIFT",1,"0:SHIFT 1:DTR 2:RTS",EX096;,EX096{P2};,"Operating Manual p126; CAT OM p8"
097,RTTY POLARITY-RX,"NOR/REV","NOR",1,"0:NORMAL 1:REVERSE",EX097;,EX097{P2};,"Operating Manual p126; CAT OM p8"
098,RTTY POLARITY-TX,"NOR/REV","NOR",1,"0:NORMAL 1:REVERSE",EX098;,EX098{P2};,"Operating Manual p126; CAT OM p8"
099,RTTY OUT LEVEL,"0-100","50",3,"000-100",EX099;,EX099{P2};,"Operating Manual p126; CAT OM p8"
100,RTTY SHIFT FREQ,"170/200/425/850 (Hz)","170Hz",1,"(Yaesu codes) 170/200/425/850",EX100;,EX100{P2};,"Operating Manual p126; CAT OM p8"
101,RTTY MARK FREQ,"1275/2125 (Hz)","2125Hz",1,"(Yaesu codes) 1275/2125",EX101;,EX101{P2};,"Operating Manual p126; CAT OM p8"
102,SSB LCUT FREQ,"OFF/100Hz-1000Hz (50Hz/step)","100Hz",2,"00:OFF 01:100Hz .. 19:1000Hz",EX102;,EX102{P2};,"Operating Manual p126; CAT OM p8"
103,SSB LCUT SLOPE,"6dB/oct / 18dB/oct","6dB/oct",1,"0:6dB/oct 1:18dB/oct",EX103;,EX103{P2};,"Operating Manual p126; CAT OM p8"
104,SSB HCUT FREQ,"700Hz-4000Hz (50Hz/step)/OFF","3000Hz",2,"00:OFF 01:700Hz .. 67:4000Hz",EX104;,EX104{P2};,"Operating Manual p127; CAT OM p8"
105,SSB HCUT SLOPE,"6dB/oct / 18dB/oct","6dB/oct",1,"0:6dB/oct 1:18dB/oct",EX105;,EX105{P2};,"Operating Manual p127; CAT OM p8"
106,SSB MIC SELECT,"MIC/REAR","MIC",1,"0:MIC 1:REAR",EX106;,EX106{P2};,"Operating Manual p127; CAT OM p8"
107,SSB OUT LEVEL,"0-100","50",3,"000-100",EX107;,EX107{P2};,"Operating Manual p127; CAT OM p8"
108,SSB PTT SELECT,"DAKY/RTS/DTR","DAKY",1,"0:DAKY 1:RTS 2:DTR",EX108;,EX108{P2};,"Operating Manual p127; CAT OM p8"
109,SSB PORT SELECT,"DATA/USB","DATA",1,"0:DATA 1:USB",EX109;,EX109{P2};,"Operating Manual p127; CAT OM p8"
110,SSB TX BPF,"100-3000/100-2900/200-2800/300-2700/400-2600","300-2700",1,"0:50-3000 1:100-2900 2:200-2800 3:300-2700 4:400-2600",EX110;,EX110{P2};,"Operating Manual p127; CAT OM p8"
111,APF WIDTH,"NARROW/MEDIUM/WIDE","MEDIUM",1,"0:NARROW 1:MEDIUM 2:WIDE",EX111;,EX111{P2};,"Operating Manual p127; CAT OM p8"
112,CONTOUR LEVEL,"-40 - 0 - 20","-15",3,"-40..+20 (P2=-40..+20)",EX112;,EX112{P2};,"Operating Manual p127; CAT OM p8"
113,CONTOUR WIDTH,"1-11","10",2,"01-11",EX113;,EX113{P2};,"Operating Manual p127; CAT OM p8"
114,IF NOTCH WIDTH,"NARROW/WIDE","WIDE",1,"0:NARROW 1:WIDE",EX114;,EX114{P2};,"Operating Manual p127; CAT OM p8"
115,SCP DISPLAY MODE,"SPECTRUM/WATERFALL","SPECTRUM",1,"0:SPECTRUM 1:WATERFALL",EX115;,EX115{P2};,"Operating Manual p127; CAT OM p8"
116,SCP SPAN FREQ,"50/100/200/500/1000 (kHz)","100kHz",2,"03:50kHz 04:100kHz 05:200kHz 06:500kHz 07:1000kHz",EX116;,EX116{P2};,"Operating Manual p127; CAT OM p8"
117,SPECTRUM COLOR,"BLUE/GRAY/GREEN/ORANGE/PURPLE/RED/SKY BLUE","BLUE",1,"0:BLUE 1:GRAY 2:GREEN 3:ORANGE 4:PURPLE 5:RED 6:SKY BLUE",EX117;,EX117{P2};,"Operating Manual p127; CAT OM p8"
118,WATER FALL COLOR,"BLUE/GRAY/GREEN/ORANGE/PURPLE/RED/SKY BLUE/MULTI","MULTI",1,"0:BLUE 1:GRAY 2:GREEN 3:ORANGE 4:PURPLE 5:RED 6:SKY BLUE 7:MULTI",EX118;,EX118{P2};,"Operating Manual p127; CAT OM p8"
119,PRMTRC EQ1 FREQ,"OFF/100-700 (100/step)","OFF",2,"00:OFF 01:100 .. 07:700Hz",EX119;,EX119{P2};,"Operating Manual p127; CAT OM p8"
120,PRMTRC EQ1 LEVEL,"-20 - 0 - 10","5",3,"-20..+10 (P2=-20..+10)",EX120;,EX120{P2};,"Operating Manual p127; CAT OM p8"
121,PRMTRC EQ1 BWTH,"1-10","10",2,"01-10",EX121;,EX121{P2};,"Operating Manual p127; CAT OM p8"
122,PRMTRC EQ2 FREQ,"OFF/700-1500 (100/step)","OFF",2,"00:OFF 01:700 .. 09:1500Hz",EX122;,EX122{P2};,"Operating Manual p127; CAT OM p8"
123,PRMTRC EQ2 LEVEL,"-20 - 0 - 10","5",3,"-20..+10 (P2=-20..+10)",EX123;,EX123{P2};,"Operating Manual p127; CAT OM p8"
124,PRMTRC EQ2 BWTH,"1-10","10",2,"01-10",EX124;,EX124{P2};,"Operating Manual p127; CAT OM p8-9"
125,PRMTRC EQ3 FREQ,"OFF/1500-3200 (100/step)","OFF",2,"00:OFF 01:1500 .. 18:3200Hz",EX125;,EX125{P2};,"Operating Manual p127; CAT OM p9"
126,PRMTRC EQ3 LEVEL,"-20 - 0 - 10","5",3,"-20..+10 (P2=-20..+10)",EX126;,EX126{P2};,"Operating Manual p127; CAT OM p9"
127,PRMTRC EQ3 BWTH,"1-10","10",2,"01-10",EX127;,EX127{P2};,"Operating Manual p127; CAT OM p9"
128,P-PRMTRC EQ1 FREQ,"OFF/100-700 (100/step)","200",2,"00:OFF 01:100 .. 07:700Hz",EX128;,EX128{P2};,"Operating Manual p127; CAT OM p9"
129,P-PRMTRC EQ1 LEVEL,"-20 - 0 - 10","0",3,"-20..+10 (P2=-20..+10)",EX129;,EX129{P2};,"Operating Manual p127; CAT OM p9"
130,P-PRMTRC EQ1 BWTH,"1-10","2",2,"01-10",EX130;,EX130{P2};,"Operating Manual p127; CAT OM p9"
131,P-PRMTRC EQ2 FREQ,"OFF/700-1500 (100/step)","800",2,"00:OFF 01:700 .. 09:1500Hz",EX131;,EX131{P2};,"Operating Manual p127; CAT OM p9"
132,P-PRMTRC EQ2 LEVEL,"-20 - 0 - 10","0",3,"-20..+10 (P2=-20..+10)",EX132;,EX132{P2};,"Operating Manual p127; CAT OM p9"
133,P-PRMTRC EQ2 BWTH,"1-10","1",2,"01-10",EX133;,EX133{P2};,"Operating Manual p127; CAT OM p9"
134,P-PRMTRC EQ3 FREQ,"OFF/1500-3200 (100/step)","2100",2,"00:OFF 01:1500 .. 18:3200Hz",EX134;,EX134{P2};,"Operating Manual p127; CAT OM p9"
135,P-PRMTRC EQ3 LEVEL,"-20 - 0 - 10","0",3,"-20..+10 (P2=-20..+10)",EX135;,EX135{P2};,"Operating Manual p127; CAT OM p9"
136,P-PRMTRC EQ3 BWTH,"1-10","1",2,"01-10",EX136;,EX136{P2};,"Operating Manual p127; CAT OM p9"
137,HF TX MAX POWER,"5-100","100",3,"005-100",EX137;,EX137{P2};,"Operating Manual p127; CAT OM p9"
138,50M TX MAX POWER,"5-100","100",3,"005-100",EX138;,EX138{P2};,"Operating Manual p127; CAT OM p9"
139,144M TX MAX POWER,"5-50","50",3,"005-050",EX139;,EX139{P2};,"Operating Manual p127; CAT OM p9"
140,430M TX MAX POWER,"5-50","50",3,"005-050",EX140;,EX140{P2};,"Operating Manual p127; CAT OM p9"
141,TUNER SELECT,"OFF/INTERNAL/EXTERNAL/ATAS/LAMP","INTERNAL",1,"0:OFF 1:INTERNAL 2:EXTERNAL 3:ATAS 4:LAMP",EX141;,EX141{P2};,"Operating Manual p127; CAT OM p9"
142,VOX SELECT,"MIC/DATA","MIC",1,"0:MIC 1:DATA",EX142;,EX142{P2};,"Operating Manual p127; CAT OM p9"
143,VOX GAIN,"0-100","50",3,"000-100",EX143;,EX143{P2};,"Operating Manual p127; CAT OM p9"
144,VOX DELAY,"30-3000 (msec)","500msec",4,"0030-3000 (10msec/step)",EX144;,EX144{P2};,"Operating Manual p127; CAT OM p9"
145,ANTI VOX GAIN,"0-100","50",3,"000-100",EX145;,EX145{P2};,"Operating Manual p127; CAT OM p9"
146,DATA VOX GAIN,"0-100","50",3,"000-100",EX146;,EX146{P2};,"Operating Manual p127; CAT OM p9"
147,DATA VOX DELAY,"30-3000 (msec)","100msec",4,"0030-3000 (10msec/step)",EX147;,EX147{P2};,"Operating Manual p127; CAT OM p9"
148,ANTI DVOX GAIN,"0-100","0",3,"000-100",EX148;,EX148{P2};,"Operating Manual p127; CAT OM p9"
149,EMERGENCY FREQ TX,"DISABLE/ENABLE","DISABLE",1,"0:DISABLE 1:ENABLE",EX149;,EX149{P2};,"Operating Manual p127; CAT OM p9"
150,PRT/WIRES FREQ,"MANUAL/PRESET","MANUAL",1,"0:MANUAL 1:PRESET",EX150;,EX150{P2};,"Operating Manual p127; CAT OM p9"
151,PRESET FREQUENCY,"(preset list/region)","145.375.00 (of 146.550.00 USA)",8,"00030000-47000000 (8 digits)",EX151;,EX151{P2};,"Operating Manual p127; CAT OM p9"
152,SEARCH SETUP,"HISTORY/ACTIVITY","HISTORY",1,"0:HISTORY 1:ACTIVITY",EX152;,EX152{P2};,"Operating Manual p127; CAT OM p9"
153,WIRES DG-ID,"AUTO/01-99","AUTO",2,"00:AUTO 01-99:DG-ID",EX153;,EX153{P2};,"Operating Manual p127; CAT OM p9"
```

### Opmerking over menu 151 (PRESET FREQUENCY)

De Operating Manual toont voorbeeld‑presets (o.a. **145.375.00** en **146.550.00 (USA)**) en suggereert daarmee regio‑afhankelijkheid. citeturn47view4  
De CAT manual geeft voor menu 151 een **8‑digit numeriek veld** met range `00030000 ~ 47000000`. Omdat Yaesu hier (in de geciteerde tabel) geen expliciete eenheid bij vermeldt, is de veiligste aanpak: **behandel dit als raw P2‑veld met 8 digits** en valideer door eerst `EX151;` te lezen en het antwoordformaat te volgen. citeturn51view2turn47view4  

## Automatiseren en implementatie‑notities

### Python (pyserial) sjabloon voor alle 153 items

Dit is een “no‑frills” patroon: stuur `EXnnn;`, lees tot `;`, en schrijf eventueel terug. De Yaesu CAT manual benadrukt dat `;` de terminator is en dat velden vaste lengtes kunnen hebben; voor parsing is “lees alles tot `;`” robuust. citeturn52view0turn51view0  
Onderstaande code is een startpunt; pas poort en baudrate aan aan jouw menu‑instelling (031 CAT RATE) en poortkeuze (typisch Enhanced COM). citeturn49view4turn31search12turn0search2  

```python
import serial
import time

def cat_query(ser: serial.Serial, cmd: str, timeout_s: float = 1.0) -> str:
    """
    Stuur een CAT-commando (incl. ';') en lees antwoord tot ';'.
    Veel FT-991A antwoorden zijn direct; sommige setups vereisen kleine delay.
    """
    if not cmd.endswith(";"):
        cmd += ";"
    ser.reset_input_buffer()
    ser.write(cmd.encode("ascii"))
    ser.flush()

    t0 = time.time()
    buf = bytearray()
    while time.time() - t0 < timeout_s:
        b = ser.read(1)
        if b:
            buf += b
            if b == b";":
                break
    return buf.decode("ascii", errors="replace")

def ex_read(ser, menu_no: int) -> str:
    cmd = f"EX{menu_no:03d};"
    ans = cat_query(ser, cmd)
    # verwacht: EXnnn....;
    if not ans.startswith(f"EX{menu_no:03d}"):
        raise RuntimeError(f"Onverwacht antwoord: {ans!r}")
    payload = ans[len(f"EX{menu_no:03d}"):-1]  # strip prefix en ';'
    return payload

def ex_set(ser, menu_no: int, p2: str) -> None:
    # p2 moet al correct geformatteerd zijn (leading zeros, teken, lengte)
    cmd = f"EX{menu_no:03d}{p2};"
    _ = cat_query(ser, cmd)  # vaak echo/antwoord; kan leeg zijn afhankelijk van setup

if __name__ == "__main__":
    # Windows: "COM12" (vaak Enhanced). Linux: "/dev/ttyUSB0" of "/dev/ttyACM0"
    port = "COM12"
    baud = 4800  # match menu 031 CAT RATE (default 4800)
    ser = serial.Serial(
        port=port,
        baudrate=baud,
        bytesize=serial.EIGHTBITS,
        parity=serial.PARITY_NONE,
        stopbits=serial.STOPBITS_TWO,
        timeout=0.1,
        xonxoff=False,
        rtscts=False,
        dsrdtr=False,
    )

    # Voorbeeld: lees menu 031 (CAT RATE)
    current = ex_read(ser, 31)
    print("Menu 031 (CAT RATE) raw P2:", current)

    # Voorbeeld: zet menu 031 naar 38400 -> P2=3 volgens Yaesu CAT tabel
    # LET OP: na zetten moet je seriële poort ook naar 38400 of je verliest de link.
    ex_set(ser, 31, "3")
    time.sleep(0.2)

    ser.close()
```

### Hamlib/newcat mapping (wat wordt wél/niet “direct” ondersteund)

Hamlib’s Yaesu “new CAT” laag bevat expliciete tabellen die voor bepaalde functies (bijvoorbeeld repeater offset) **EX0xx‑commando’s per rig‑model kiezen** en herkent FT‑991A als rig‑ID. Dat illustreert dat open‑source stacks vaak **slechts een subset** van menu‑achtige instellingen abstraheren en dat je voor “alle 153 items” meestal een eigen EX‑laag nodig hebt. citeturn19view2  
Voor specifieke parameters zoals ANTIVOX merkt Hamlib op dat sommige rigs verschillende EX‑menu items gebruiken voor set/get (voorbeeld in codecommentaar), wat een indicatie is dat er tussen modellen variatie kan zitten en dat “EX menu‑nummers” soms model‑specifiek geïnterpreteerd moeten worden. citeturn19view2  

## Firmware/compatibiliteit, valkuilen en troubleshooting

### Firmware/regio‑caveats

De Operating Manual menu‑tabel bevat een expliciete voetnoot die wijst op **regio‑specifieke verschillen** (“European Version”). Behandel daarom defaults/available settings als mogelijk regio‑afhankelijk. citeturn49view4  
De FT‑991A CAT manual is een FT‑991A document maar draagt in de tekst “FT‑991 CAT Operation Reference Book” en wordt in de praktijk ook als familie‑document gebruikt; vertrouw voor jouw rig op de **FT‑991A** entries en test altijd met `EXnnn;` readback. citeturn52view0turn51view0  

### Bekende CAT‑valkuilen bij EX menu

Gebruik altijd `;` als terminator en stuur exact het juiste aantal digits; Yaesu noemt expliciet voorbeelden van “te weinig digits” en “te veel digits” als fout. citeturn52view0turn51view0  
Let extra op menu‑items met niet‑intuïtieve codering (bijv. **028 GPS/232C SELECT** gebruikt 0/1/3; **072/077 PORT SELECT** gebruikt in de CAT‑tabel “1:DATA 2:USB” in plaats van 0/1). citeturn51view2turn51view0turn51view1  
Als je **031 CAT RATE** wijzigt, moet je PC‑zijde daarna op dezelfde baudrate doorgaan; de mogelijke baudrates staan in de menu‑tabel en de P2‑codes staan in de CAT‑tabel. citeturn49view4turn51view1  

### Troubleshooting checklist

- **Geen antwoord op `EXnnn;`**: controleer poortkeuze (USB Enhanced), baudrate match met menu 031, en terminator `;`. citeturn31search12turn49view4turn52view0  
- **Garbled/rommel**: framing mismatch (probeer 8N2 zonder flow control als compatibel startpunt) en/of verkeerde baudrate. citeturn0search2turn49view4  
- **Antwoord komt wel, maar “verkeerde” lengte**: sommige menu’s hebben P2‑velden met leading zeros of sign; parse tot `;` en valideer lengte tegen “P2 Digits” uit de EX‑tabel. citeturn51view0turn51view1turn51view2  
- **Na wijzigen CAT RATE is de link dood**: verwacht; reconnect op nieuwe baudrate (P2‑code). citeturn49view4turn51view1  
- **Menu‑item lijkt niet te veranderen**: sommige velden zijn read‑only of functioneel gebonden (bijv. RADIO ID). Lees terug na set; als het niet wijzigt, behandel als read‑only. citeturn47view4turn51view1  

### Korte test‑checklist (veilig “read‑only”)

- Lees `EX031;` (CAT RATE) en log antwoord. citeturn51view0turn49view4  
- Lees `EX143;` (VOX GAIN) en log antwoord. citeturn51view2turn47view4  
- Lees `EX001;` (AGC FAST DELAY) en log antwoord; controleer of het 4 digits teruggeeft. citeturn51view0turn49view4  

### Voorbeeld “read‑modify‑write” sessie (menu 143 VOX GAIN)

Doel: VOX GAIN van 50 naar 60 en terugverifiëren.  
- Operating Manual: menu 143 is **0–100**, default **50**. citeturn47view4  
- CAT manual: menu 143 “VOX GAIN” is **000–100** met **P2 Digits = 3**. citeturn51view2  

Sessiestappen:
1. Read `EX143;` → verwacht `EX143050;` (voorbeeld als default actief).  
2. Set `EX143060;`  
3. Read `EX143;` → verwacht `EX143060;`  

(ASCII‑hex voor `EX143060;`: `45 58 31 34 33 30 36 30 3B`.)

---

**Beperkingen van deze output**: je vroeg om een “downloadable” CSV/JSON. Door tool‑beperkingen in deze chat kan ik geen bijlage genereren; daarom is de volledige machine‑readable CSV hierboven **inline** opgenomen zodat je die direct kunt kopiëren en opslaan.