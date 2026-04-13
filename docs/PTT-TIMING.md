# PTT Timing — Implementatie

## Overzicht

ThetisLink gebruikt een **jitter-buffer-output trigger** voor PTT timing. De PTT flag reist als metadata mee in elk audio frame door dezelfde jitter buffer als de audio. Het moment dat het eerste PTT=true frame de jitter buffer **verlaat** is de trigger voor TX activatie.

## Signaalpad

PTT en audio reizen in hetzelfde UDP packet — het netwerk introduceert geen differentieel delay.

### TX start

```
Client:   PTT knop → audio frame met PTT=true flag → Opus encode → UDP packet

Server:   UDP packet → push in jitter buffer (inclusief PTT flag)
          playout tick → pull frame → PTT=true gedetecteerd
          → ZZTU1/ZZTX1 via TCI/CAT naar Thetis
          → audio frame naar TCI TX_AUDIO_STREAM
```

De audio en het PTT commando verlaten de jitter buffer op hetzelfde moment, waardoor ze synchroon bij Thetis aankomen.

### TX release

Bij PTT loslaten start een **tail delay** die wacht tot resterende audio frames zijn uitgespeeld voordat `ZZTX0;` wordt gestuurd.

### Capture gate

Op de client wordt bij TX start de playback van RX1/RX2 Thetis audio gestopt (in de mix loop, niet via hardware mute — zodat Yaesu audio door blijft spelen). De capture gate opent na een korte delay (2 ticks = 40ms) zodat de speaker eerst kan leeglopen en geen feedback ontstaat.

## Delay componenten

| Component | Delay | Type |
|-----------|-------|------|
| Opus frame size | 20ms | Statisch (protocol) |
| Playout tick interval | 20ms | Statisch |
| Jitter buffer depth | 60-400ms | Dynamisch (jitter-adaptief) |
| TCI audio delivery | ~0ms | Localhost WebSocket |
| **Totale onzekerheid** | **~0ms** | PTT en audio zijn gesynchroniseerd |

## Jitter buffer bij TX start

De server jitter buffer wordt **gereset** bij elke nieuwe TX sessie. Reset zet `target_depth` terug naar `min_depth` (3 frames = 60ms) en `jitter_estimate` naar 0.

## TX spectrum override

Tijdens TX past de client het spectrum display aan:
1. **PTT aan:** Huidige ref, range en auto_ref opslaan → auto_ref uit → ref = -30 dB → range = 120 dB
2. **PTT uit:** ref en range direct herstellen → auto_ref na 200ms (desktop) of 500ms (Android) herstellen met EMA reset

Dit geeft een optimale weergave van het TX signaal zonder dat de RX instellingen verloren gaan.
