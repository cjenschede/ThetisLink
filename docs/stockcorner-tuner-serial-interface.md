# Stockcorner JC-4s Tuner — Serieel Interface via USB

## Overzicht

De Stockcorner JC-Control kastje (bij de JC-4s antenne tuner) is aangepast om extern aangestuurd te worden via een USB-naar-serieel adapter. Er worden **geen seriële data** verstuurd — alleen de **modem control lines** (RTS/CTS) worden gebruikt als digitale signalen.

## Hardware aansluitingen

### Twee signalen

| Signaal | Richting | Functie |
|---------|----------|---------|
| **RTS** (Request To Send) | PC → Tuner | Simuleert het indrukken van de Start/Tune knop |
| **CTS** (Clear To Send) | Tuner → PC | Leest de rode Tune LED status uit |

### JC-Control kast aanpassing

1. **Start knop (RTS)**: De Start-knop op de JC-Control kan via een simpel schakelcircuit kortgesloten worden. Het RTS signaal van de USB-serieel adapter schakelt dit circuit, waardoor de tuner begint met tunen — alsof je de knop indrukt.

2. **Tune LED (CTS)**: De rode Tune LED op de JC-Control wordt uitgelezen. Dit signaal gaat naar de CTS lijn van de USB-serieel adapter. Zolang de tuner bezig is met tunen brandt de LED (CTS = HIGH), wanneer het tunen klaar is gaat de LED uit (CTS = LOW).

### USB-serieel adapter

Een standaard USB-naar-serieel (TTL) printje verbindt deze twee signalen met de server PC. Alleen de RTS en CTS lijnen worden gebruikt, plus GND. De TX/RX data lijnen worden niet aangesloten.

```
USB-Serieel adapter          JC-Control kast
─────────────────          ─────────────────
RTS ──────────────────────→ Start knop (schakelcircuit)
CTS ←────────────────────── Tune LED (uitgelezen)
GND ──────────────────────→ GND
```

## Software protocol

De ThetisLink Server (`tuner.rs`) opent de COM-poort op 9600 baud en gebruikt uitsluitend de modem control lines:

### Initialisatie

1. DTR HIGH zetten (voeding/ready signaal)
2. RTS HIGH voor 200ms, dan RTS LOW — wake-up puls voor de JC-4s

### Tune sequentie

```
Stap  Actie                          Signaal
────  ─────                          ───────
1     RTS HIGH                       Tuner voorbereiden
2     Wacht 150ms
3     ZZTU1 naar Thetis (CAT)        Tune carrier AAN (CW draaggolf)
4     Wacht 500ms (carrier opstart)
5     RTS LOW                        Start het tunen
6     Wacht op CTS = TRUE            Tuner is begonnen
7     Wacht op CTS = FALSE           Tunen klaar (LED uit)
8     ZZTU0 naar Thetis (CAT)        Tune carrier UIT
```

### Timeout en abort

- Als CTS niet binnen 3 seconden TRUE wordt na RTS LOW → timeout
- Als het tunen langer duurt dan 30 seconden → timeout
- Bij abort: ZZTU0 sturen en RTS LOW zetten

### Safe tune (PA bescherming)

Als er een eindversterker (SPE Expert of RF2K-S) is aangesloten, wordt deze automatisch in Standby gezet voordat het tunen begint, en na afloop weer in Operate.

## Server configuratie

In het ThetisLink Server configuratiebestand:

```
tuner_port=COM5
tuner_enabled=true
```

Of via command line:

```
ThetisLink-Server.exe --tuner-port COM5
```

## Status in de UI

De tuner status wordt weergegeven in zowel de server UI als de remote clients:

| State | Betekenis | Kleur |
|-------|-----------|-------|
| 0 - Idle | Klaar voor tunen | — |
| 1 - Tuning | Bezig met tunen | Blauw |
| 2 - Done OK | Succesvol getuned | Groen |
| 3 - Timeout | Tunen duurde te lang | Rood |
| 4 - Aborted | Tunen afgebroken | Geel |

De "Done OK" status wordt "stale" (oranje) als de VFO meer dan 25 kHz verschuift ten opzichte van de getuunde frequentie.
