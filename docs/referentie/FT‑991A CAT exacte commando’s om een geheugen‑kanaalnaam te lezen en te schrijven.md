# FT‑991A CAT: exacte commando’s om een geheugen‑kanaalnaam te lezen en te schrijven

## Executive summary

De Yaesu FT‑991A gebruikt een **ASCII‑gebaseerd CAT‑protocol**: elk commando bestaat uit **twee letters**, vaste‑lengte parameters en wordt beëindigd met een **puntkomma (`;`)**. citeturn2view0turn8view1  
De **geheugen‑kanaalnaam** (“Memory Tag”, kanaalnaam) is **geen los ‘NAME’ commando**, maar zit als **TAG‑veld (max. 12 ASCII‑tekens)** in het commando **`MT` (MEMORY CHANNEL WRITE/TAG)**. citeturn28view1turn4view0  
Concreet:

- **Lezen van de kanaalnaam**: stuur `MTnnn;` (met `nnn` = kanaalnummer 001–117). De radio antwoordt met een volledige `MT…;` regel waarin de **laatste 12 tekens vóór de puntkomma** de TAG/kanaalnaam zijn. citeturn28view1  
- **Schrijven van de kanaalnaam**: je móet een volledige `MT…TAG…;` *set*‑string sturen (vast formaat), inclusief frequentie/mode/CTCSS‑status etc. Er is geen “set‑tag‑only” command; je doet dus typisch **read‑modify‑write** (lees `MT`, wijzig alleen de tag, schrijf volledige `MT`). citeturn28view1  

Qua transport is de **USB‑verbinding** (CP210x “Dual UART Bridge”) het meest praktisch. Yaesu documenteert dat je dan twee COM‑poorten krijgt: **Enhanced COM Port** voor **CAT communications** (frequentie/mode etc) en **Standard COM Port** voor **TX Controls** (PTT/keying/digital). Voor memory‑tag commando’s gebruik je dus normaliter de **Enhanced COM Port**. citeturn13view0  

## CAT‑commando’s voor memory tag: exact formaat, bytes en voorbeelden

### Protocolbasis die je nodig hebt

Yaesu beschrijft het algemene CAT‑format als: **Command (2 letters) + Parameters + Terminator (`;`)**. citeturn2view0turn8view1  
Belangrijk detail: in de handleiding worden commando’s soms met spaties getoond voor leesbaarheid, maar de feitelijke string is aaneengesloten, zoals het voorbeeld “`FA014250000;`”. citeturn2view0turn8view1  

### Waar zit de kanaalnaam in de FT‑991A?

De kanaalnaam is het **P12 “TAG Characters”** veld van het **`MT` commando** en is **tot 12 ASCII‑tekens**. citeturn28view1turn4view0  
Het `MT` commando werkt voor geheugenkanalen **001–117**. citeturn28view1turn28view0  

### Klein vergelijkingstabel: read vs write

| Actie | Commando | Exacte ASCII string | Bytes (ASCII hex) | Opmerking |
|---|---|---|---|---|
| Lees kanaalnaam (en alle geheugenvelden) | `MT` read | `MTnnn;` | `4D 54` + `nnn` + `3B` | Antwoord bevat TAG als 12 tekens. citeturn28view1 |
| Zet kanaalnaam | `MT` set | `MT` + `<P1..P11>` + `<12‑char TAG>` + `;` | ASCII‑bytes van gehele string | Je moet de *hele* MT‑payload sturen; TAG is max 12 ASCII. citeturn28view1 |

### Exact “read memory name” commando

**ASCII**  
- Lees kanaal 1: `MT001;` citeturn28view1  
- Lees kanaal 12: `MT012;` citeturn28view1  
- Lees kanaal 117: `MT117;` citeturn28view1turn28view0  

**Hex (ASCII bytes)**  
- `MT001;` = `4D 54 30 30 31 3B`  
- `MT117;` = `4D 54 31 31 37 3B`

**Antwoordformaat (essentie)**  
De radio antwoordt met een string die opnieuw met `MT` begint en eindigt met `;`, en waarin `P12` de **TAG** is (12 tekens). citeturn28view1  

De MT‑tabel laat zien dat de TAG‑tekens (`P12`) **posities 29–40** van de antwoordstring innemen (0‑gebaseerd: indices 28–39), direct gevolgd door `;`. citeturn28view1  

### Exact “write/set memory name” commando

De volledige `MT` set‑string heeft vaste velden:

`MT` + `P1`(3) + `P2`(9) + `P3`(5) + `P4`(1) + `P5`(1) + `P6`(1) + `P7`(1) + `P8`(1) + `P9`(2) + `P10`(1) + `P11`(1) + `P12`(12) + `;` citeturn28view1  

Waarbij:
- `P12` = **TAG Characters (up to 12 characters) (ASCII)** citeturn28view1turn4view0  
- `P7` is bij **Set** “Fixed” (0), terwijl bij **Read/Answer** `P7` een status is (0=VFO, 1=Memory). Dit is een subtiele maar belangrijke valkuil voor replay: een “read response” is niet per definitie 1‑op‑1 herbruikbaar als “set”, omdat `P7` semantisch verschilt. citeturn28view1turn19view0  

#### Voorbeeld: kanaal 001 hernoemen naar “REPEATER1”

Stel je wilt kanaal 001 behouden qua instellingen en alleen de tag aanpassen. Dan doe je:

1) `MT001;` → ontvang volledige status (incl. bestaande freq/mode/etc + huidige TAG). citeturn28view1  
2) bouw een set‑string met **dezelfde P1..P11** en nieuwe `P12`. citeturn28view1  

**Voorbeeld set‑string (illustratief)**  
Onderstaand is een **syntactisch correct** voorbeeld (niet “waarheidsgetrouw” voor jouw kanaal-inhoud, want die hangt af van jouw geheugen). Het voorbeeld toont het vaste format:

- `P1` = `001`  
- `P2` (VFO‑A Frequency Hz) = `145500000` (145.500 MHz)  
- `P3` Clarifier dir+offset = `+0000`  
- `P4` RX CLAR = `0`  
- `P5` TX CLAR = `0`  
- `P6` MODE = `4` (FM)  
- `P7` Set fixed = `0` citeturn28view1  
- `P8` CTCSS/DCS = `0` (OFF)  
- `P9` fixed = `00`  
- `P10` shift = `0` (Simplex)  
- `P11` fixed = `0`  
- `P12` TAG = `"REPEATER1   "` (12 tekens, met 3 spaties padding) citeturn28view1  

**ASCII string**  
`MT001145500000+00000040000REPEATER1   ;`

**Hex (ASCII bytes, begin/einde)**  
- Begin: `4D 54 30 30 31 31 34 35 ...`  
- Tag `REPEATER1   `: `52 45 50 45 41 54 45 52 31 20 20 20`  
- Terminator: `3B`

> Let op: omdat de TAG “up to 12 characters” is maar de MT‑payload vaste posities heeft, is in de praktijk **padding met spaties** de veiligste manier om <12 tekens te vullen, zodat de totale lengte klopt (je wilt precies 12 tag‑bytes in de string). citeturn28view1  

## Seriële/USB‑instellingen en juiste COM‑poort

### Welke COM‑poort gebruik je op USB?

Bij USB‑CAT krijgt de FT‑991A/SCU‑17 twee virtuele COM‑poorten:

- **Enhanced COM Port**: “CAT Communications (Frequency and Communication Mode Settings) and firmware updating” citeturn13view0  
- **Standard COM Port**: “TX Controls (PTT control, CW Keying, Digital Mode Operation)” citeturn13view0  

Voor **MT‑memory tag read/write** is dit functioneel **CAT‑communicatie**, dus gebruik in de praktijk **Enhanced COM Port**. citeturn13view0  

### Baudrate en Yaesu‑menu’s die dit bepalen

In de FT‑991A CAT manual staat een menu‑tabel met o.a.:

- **CAT RATE**: 4800 / 9600 / 19200 / 38400 bps citeturn5view0turn10view2  
- **CAT TOT** (timeout): 10 ms / 100 ms / 1000 ms / 3000 ms citeturn5view0  
- **CAT RTS**: DISABLE / ENABLE citeturn5view0turn7view4  

Voor RS‑232C via de CAT‑jack moet je bovendien “GPS/232C SELECT” naar **RS232C** zetten. citeturn2view0turn8view0  

### Databits/parity/stopbits/flow control: wat is “juist” in de praktijk?

Yaesu’s FT‑991(A) CAT reference manual specificeert expliciet de **baudrate via menu**, maar (zoals vaker bij Yaesu) is de framing niet altijd even expliciet in dit document. citeturn5view0turn28view1  
Daarom is het nuttig om te kijken naar gevestigde implementaties en richtlijnen:

- **Hamlib** (FT‑991 backend) initialiseert CAT typisch als **8 databits, geen parity, 2 stopbits, hardware handshake**; in de bron staat expliciet `serial_data_bits = 8`, `serial_stop_bits = 2`, `serial_parity = NONE`, `serial_handshake = HARDWARE`. citeturn17view0  
- **DXLab Suite wiki** beschrijft dat “most Yaesu transceivers require **2 stop bits**” en noemt FT‑991 expliciet in de groep “recent Yaesu transceivers”. citeturn24search9  
- **flrig** (FT‑991A rigdef) gebruikt in code `serial_baudrate = BR38400; stopbits = 1; serial_rtscts = true;` — wat laat zien dat sommige stacks met **1 stopbit** ook werken (driver/hardware toleranties), maar dit kan verschillen per OS/USB‑driver. citeturn22view0  

**Aanbevolen startconfiguratie (praktisch, diagnostisch):**
- Baudrate: **38400** (en zet FT‑991A Menu CAT RATE op hetzelfde). citeturn5view0turn24search9  
- Data bits: **8**, parity: **None** (N) (breed gangbaar in ham CAT en consistent met Hamlib). citeturn17view0  
- Stop bits: begin met **2** (compatibel met DXLab + Hamlib), en als je geen respons krijgt, probeer **1** (zoals flrig). citeturn24search9turn17view0turn22view0  
- Flow control: als **CAT RTS = ENABLE** op de radio staat, gebruik **RTS/CTS**; als CAT RTS uit staat, zet flow control uit. citeturn7view4  

### Belangrijke praktijkvalkuil: CAT TOT te laag voor “handmatig typen”

Als je met een terminal‑emulator (PuTTY/RealTerm/minicom) handmatig commando’s typt, kan de radio’s **CAT Timeout (CAT TOT)** te agressief zijn (milliseconden), waardoor je “geen reactie” ervaart terwijl de set wel werkt met echte software. In de FT‑991A community wordt expliciet aangeraden CAT TOT te checken/verhogen voor terminalgebruik. citeturn24search8turn5view0  

## Voorbeelden met tools en Python (pyserial)

### Windows: PuTTY en RealTerm

**PuTTY (Serial)**
1. Kies de **Enhanced COM Port (COMx)**. citeturn13view0  
2. Stel baudrate gelijk aan menu **CAT RATE** (bijv. 38400). citeturn5view0  
3. Stel data bits/parity/stopbits (start met 8‑N‑2; probeer 8‑N‑1 indien nodig). citeturn24search9turn22view0turn17view0  
4. Typ `MT001;` en druk Enter (Enter stuurt meestal CR/LF, maar Yaesu kijkt primair naar `;` als terminator; CR/LF mag erachter staan zolang `;` aanwezig blijft). citeturn8view1turn28view1  

**RealTerm**
- Zet “Display” op **ASCII** en “Send” op **ASCII**; stuur exact `MT001;`.  
- Gebruik “Capture” om de reply te loggen, zodat je exact 12 tag‑tekens kunt tellen.

### Linux: `screen` en `minicom`

**screen**
- `screen /dev/ttyUSB0 38400`  
- Type: `MT001;`  
- Sluit met `Ctrl‑A` → `\`.

**minicom**
- Configureer seriële parameters in `minicom -s`.  
- Zet hardware flow control passend bij “CAT RTS”. citeturn7view4  
- Test met `MT001;`.

Tip: voor handmatig typen op Linux geldt dezelfde CAT‑timeout valkuil; verhoog **CAT TOT** als je “geen antwoord” ziet. citeturn24search8turn5view0  

### Python (pyserial): read‑modify‑write cycle

Onderstaande code is bedoeld als **praktisch referentie‑script**. Het gaat uit van:
- geen specifieke constraint qua OS/firmware,
- Enhanced COM port voor CAT,
- lezen met `MTnnn;`,
- de MT‑reply parsen op vaste posities volgens Yaesu’s tabel. citeturn28view1turn13view0  

```python
import serial
from dataclasses import dataclass

@dataclass
class MTRecord:
    mem: str          # 3 digits, e.g. "001"
    freq_hz: int      # 9 digits
    clar: str         # 5 chars, e.g. "+0000"
    rx_clar: str      # "0" or "1"
    tx_clar: str      # "0" or "1"
    mode: str         # single char (1..E)
    p7: str           # Set: fixed "0"; Answer: "0"=VFO, "1"=Memory
    tone_mode: str    # P8
    fixed00: str      # "00"
    rpt_shift: str    # P10
    fixed0: str       # P11
    tag12: str        # 12 chars (may include spaces)

def read_until_semicolon(ser: serial.Serial, timeout_s: float = 1.0) -> str:
    ser.timeout = timeout_s
    buf = bytearray()
    while True:
        b = ser.read(1)
        if not b:
            raise TimeoutError("Timeout: geen ';' ontvangen")
        buf += b
        if b == b';':
            return buf.decode('ascii', errors='replace')

def mt_read(ser: serial.Serial, mem_no: int) -> MTRecord:
    cmd = f"MT{mem_no:03d};"
    ser.write(cmd.encode("ascii"))
    reply = read_until_semicolon(ser)
    if not reply.startswith("MT") or len(reply) < 41:
        raise ValueError(f"Onverwachte reply: {reply!r}")

    # Posities afgeleid uit Yaesu MT tabel (MT antwoord, 41 chars incl ';')
    # 0-1: "MT"
    mem = reply[2:5]
    freq_hz = int(reply[5:14])
    clar = reply[14:19]
    rx_clar = reply[19]
    tx_clar = reply[20]
    mode = reply[21]
    p7 = reply[22]
    tone_mode = reply[23]
    fixed00 = reply[24:26]
    rpt_shift = reply[26]
    fixed0 = reply[27]
    tag12 = reply[28:40]  # 12 chars
    return MTRecord(mem, freq_hz, clar, rx_clar, tx_clar, mode, p7, tone_mode, fixed00, rpt_shift, fixed0, tag12)

def mt_write_tag(ser: serial.Serial, rec: MTRecord, new_tag: str) -> None:
    # Yaesu: TAG up to 12 chars ASCII
    # Veilig: forceer ASCII en pad met spaties tot exact 12
    new_tag_ascii = new_tag.encode("ascii", errors="ignore").decode("ascii")
    if len(new_tag_ascii) > 12:
        new_tag_ascii = new_tag_ascii[:12]
    tag12 = new_tag_ascii.ljust(12, " ")

    # LET OP: P7 is bij Set "fixed 0" (niet de read-status).
    # Gebruik dus altijd "0" voor P7 in een set-commando.
    p7_set = "0"

    cmd = (
        "MT"
        f"{rec.mem}"
        f"{rec.freq_hz:09d}"
        f"{rec.clar}"
        f"{rec.rx_clar}"
        f"{rec.tx_clar}"
        f"{rec.mode}"
        f"{p7_set}"
        f"{rec.tone_mode}"
        f"{rec.fixed00}"
        f"{rec.rpt_shift}"
        f"{rec.fixed0}"
        f"{tag12}"
        ";"
    )
    if len(cmd) != 41:
        raise ValueError(f"MT set command heeft onverwachte lengte {len(cmd)}: {cmd!r}")

    ser.write(cmd.encode("ascii"))

    # Veel Yaesu CAT commands geven geen ACK; verifieer door terug te lezen:
    verify = mt_read(ser, int(rec.mem))
    if verify.tag12 != tag12:
        raise RuntimeError(f"Verificatie faalt: tag in radio={verify.tag12!r}, verwacht={tag12!r}")

def main():
    # Pas aan: COM-poort van Enhanced COM Port.
    port = "COM8"        # Windows voorbeeld
    baud = 38400         # match FT-991A Menu CAT RATE
    with serial.Serial(port=port, baudrate=baud, bytesize=8, parity="N", stopbits=2, rtscts=True) as ser:
        # 1) Lees
        rec = mt_read(ser, 1)
        print("Huidige TAG:", rec.tag12)

        # 2) Wijzig
        mt_write_tag(ser, rec, "REPEATER1")
        print("TAG bijgewerkt.")

if __name__ == "__main__":
    main()
```

**Waarom deze code zo is opgebouwd (koppeling aan bronnen)**  
- De vaste veldvolgorde en de 12‑char TAG komen rechtstreeks uit het `MT` schema (“TAG Characters (up to 12 characters) (ASCII)”). citeturn28view1  
- Het feit dat `P7` bij *Set* “Fixed” is maar bij *Read/Answer* status weergeeft, verklaart waarom een read‑reply niet altijd 1‑op‑1 teruggeschreven kan worden; dit komt ook terug in praktijknotities van ontwikkelaars (“answer … cannot be replayed without modification”). citeturn28view1turn19view0  

### Voorbeeld “volle sessie”: read → modify → write → read verify

Een minimale sessie (conceptueel):

```text
TX> MT001;
RX< MT001145500000+00000040000OLDNAME     ;
TX> MT001145500000+00000040000REPEATER1   ;
TX> MT001;
RX< MT001145500000+00000040000REPEATER1   ;
```

De exacte bytes in RX hangen af van jouw opgeslagen channel‑parameters; het belangrijke patroon is dat de TAG altijd 12 karakters is en dat de reply eindigt op `;`. citeturn28view1  

## FT‑991 vs FT‑991A, firmware‑/ecosysteemverschillen en bekende valkuilen

### FT‑991 vs FT‑991A: is `MT` hetzelfde?

Ja: zowel de FT‑991 als de FT‑991A CAT manuals beschrijven `MT` als **MEMORY CHANNEL WRITE/TAG** met **TAG Characters (up to 12 characters) (ASCII)** en dezelfde type velden. citeturn27view1turn28view1  
Voor het lezen/schrijven van kanaalnamen kun je het `MT` mechanisme dus als functioneel gelijk beschouwen tussen FT‑991 en FT‑991A.

### Waarom bestaat er tóch discussie over memory programming?

In de praktijk ervaren tools soms dat:
- niet alle geheugenvelden via de gedocumenteerde CAT‑commando’s volledig te manipuleren zijn, en/of  
- “round‑trip” (read → write exact terug) niet werkt zonder aanpassingen.

Dat zie je bijvoorbeeld terug in:
- een community‑note dat `MT/MW` wel gedocumenteerd zijn maar dat een read‑antwoord “niet replaybaar” is zonder modificatie. citeturn19view0  
- een CHIRP issue waarin expliciet gevraagd wordt hoe je de “memory tag” via CAT schrijft/leest, en waar een RT Systems “write” handshake wordt getoond die óók **undocumented** `SP…` commando’s (`SPID`, `SPR`, `SPW`) gebruikt. Dit suggereert dat commerciële programmeersoftware soms buiten het publieke CAT‑subset gaat om alles te kunnen zetten. citeturn21search6turn19view0  

Voor jouw doel (alleen kanaalnaam) is de officiële route via `MT` doorgaans voldoende, maar het verklaart waarom sommige ecosystemen (zoals CHIRP) terughoudend zijn: volledige “channel cloning/programming” kan meer vereisen dan enkel `MT/MW`. citeturn21search6turn19view0  

## Troubleshooting en testchecklist

### Veelvoorkomende failures en oplossingen

Geen response op `MT001;`
- Controleer of je de **Enhanced COM Port** gebruikt, niet de Standard COM Port. citeturn13view0  
- Match baudrate met radio menu **CAT RATE**. citeturn5view0  
- Verhoog **CAT TOT** als je handmatig test via terminal emulator; te korte timeout geeft “stilte”. citeturn24search8turn5view0  
- Check terminator: elke CAT‑command eindigt op `;`. Zonder `;` is het commando niet “af”. citeturn8view1turn2view0  

Garbled/rare tekens in reply
- Framing mismatch: probeer 8‑N‑2 (vaak Yaesu), en als dat niet werkt 8‑N‑1 (sommige implementaties). citeturn24search9turn17view0turn22view0  
- Flow control mismatch: zet RTS/CTS consistent met menu “CAT RTS”. citeturn7view4  

Kanaalnaam wordt niet gezet of “verkeerd”
- TAG is max **12 ASCII‑tekens**. Niet‑ASCII karakters (accenten/UTF‑8) kunnen problemen geven; strip naar puur ASCII. citeturn28view1  
- Zorg dat je bij set de volledige MT‑payload stuurt en dat de TAG exact 12 tekens is (pad met spaties). Het schema toont dat TAG vaste posities vult. citeturn28view1  
- Let op `P7`: bij set is dit “Fixed 0”; bij answer kan het een statuswaarde zijn. Dit is één van de redenen dat blind “reply terugsturen” mis kan gaan. citeturn28view1turn19view0  

### Korte testchecklist

1. Radio menu: CAT RATE = 38400 (of jouw keuze), CAT RTS passend, CAT TOT niet te laag. citeturn5view0turn7view4  
2. PC: juiste COM‑poort (**Enhanced**) en matching serial settings. citeturn13view0  
3. Stuur `FA;` (eenvoudige read) om te verifiëren dat CAT werkt; Yaesu geeft voorbeeld dat dit een antwoord oplevert. citeturn2view0turn8view1  
4. Stuur `MT001;` en check dat je een `MT…;` reply krijgt. citeturn28view1  
5. Parse de laatste 12 tekens vóór `;` als tag. citeturn28view1  
6. Wijzig tag (≤12 ASCII), pad naar 12 en stuur volledige set‑`MT…;`. citeturn28view1  
7. Verifieer door opnieuw `MT001;` te sturen.

## Bronnen en links

Primaire Yaesu‑bronnen:
- Yaesu **FT‑991A CAT Operation Reference Manual** (officiële commandotabellen; `MT` met TAG‑veld; terminator `;`; menu items CAT RATE/TOT/RTS). citeturn2view0turn28view1turn5view0  
- Yaesu **FT‑991 CAT Operation Reference Manual** (vergelijking: `MT`/TAG is inhoudelijk gelijk). citeturn26view0turn27view1  
- Yaesu **Virtual COM Port Driver Installation Manual** (Enhanced vs Standard COM port rolverdeling). citeturn13view0  

Open‑source implementaties/observaties:
- **Hamlib** FT‑991 backend: serial defaults (8 databits, none parity, 2 stopbits, hardware handshake). citeturn17view0  
- **flrig** FT‑991A rigdef (serial parameters in code: o.a. stopbits=1, RTS/CTS true). citeturn22view0  
- Community‑note over `MT/MW` “niet replaybaar zonder modificatie” + verwijzing naar CHIRP/FLrig. citeturn19view0  
- CHIRP issue #2531: vraag rond memory tag + RT Systems handshake met `SP…` commando’s. citeturn21search6  

Aanvullende (reputabele) praktijkrichtlijnen:
- DXLab Suite wiki: “most Yaesu transceivers require 2 stop bits” + FT‑991 in recente Yaesu groep. citeturn24search9  
- FT‑991A community tip: verhoog CAT Timeout voor terminal emulator. citeturn24search8