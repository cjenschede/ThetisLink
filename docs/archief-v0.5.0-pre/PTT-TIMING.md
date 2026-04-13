# PTT Timing Analyse & Implementatieplan

## Probleem

Bij TX start stuurt de server `ZZTX1;` naar Thetis zodra het eerste PTT=true packet binnenkomt. Maar de audio uit datzelfde packet moet nog door de jitter buffer → Opus decode → resample → cpal → VB-Cable voordat Thetis het hoort. In die tussentijd zendt Thetis ruis.

Bij TX release is het omgekeerd: `ZZTX0;` moet wachten tot alle audio is uitgespeeld.

## Signaalpad analyse

### Kernfeit: PTT en audio reizen in hetzelfde UDP packet

Het netwerk introduceert **geen** differentiële delay — PTT flag en audio data komen altijd tegelijk aan op de server. Het verschil ontstaat volledig **server-side**.

### TX start: audio delay t.o.v. PTT

Na aankomst op de server:

```
PTT pad:    packet → update_from_packet() → ZZTX1; via CAT TCP → Thetis    (~0ms)
Audio pad:  packet → jitter buffer fill → playout → decode → cpal → VB-Cable → Thetis  (60-100ms)
```

| Component | Delay | Type |
|-----------|-------|------|
| Jitter buffer fill (min_depth=3) | 40-60ms | Deterministisch na reset (altijd min_depth) |
| Playout tick fase-alignering | 0-20ms | Per-activatie variabel (onafhankelijke 20ms klokken) |
| cpal WASAPI playback buffer | ~10-20ms | Per-systeem constant, buiten controle |
| VB-Cable loopback | ~0ms | Verwaarloosbaar (kernel virtual device) |
| Thetis WASAPI capture buffer | ~10-20ms | Per-systeem constant, buiten controle |
| **Totaal** | **60-100ms** | **~40ms bandbreedte** |

### TX release: resterende audio in pipeline

Bij PTT loslaten zitten er nog frames in de jitter buffer + cpal pipeline. De tail delay moet lang genoeg zijn om alles uit te laten spelen.

Benodigde tail delay = `jitter_buffer.depth() × 20ms` + cpal + VB-Cable (~30ms marge)

## Delay categorisatie

| Delay | Waarde | Type |
|-------|--------|------|
| Opus frame size | 20ms | Absoluut statisch (protocol) |
| Playout tick interval | 20ms | Absoluut statisch |
| Server jitter buffer min_depth | 3 frames (60ms) | Statisch hardcoded |
| Server jitter buffer max_depth | 20 frames (400ms) | Statisch hardcoded |
| Server jitter buffer target_depth | 60-400ms | Dynamisch (jitter-adaptief) |
| cpal WASAPI buffers | ~10-20ms per richting | Buiten controle (OS/driver) |
| Netwerk (UDP) | variabel | Dynamisch, maar gelijk voor PTT en audio |

### Jitter buffer bij TX start

De server jitter buffer wordt **gereset** bij elke nieuwe TX sessie (`jitter_buf.reset()` in network.rs). Reset zet `target_depth` terug naar `min_depth` (3) en `jitter_estimate` naar 0. Daarom is de jitter buffer delay bij TX start altijd gebaseerd op `min_depth`, niet op de dynamische waarde. De dynamische adaptatie groeit pas tijdens een lopende sessie.

## Overwogen aanpak: embedded signaal

Idee: een DC offset of ultrasone toon meesturen met de audio, zodat PTT en audio exact hetzelfde fysieke pad volgen. Aan de ontvangende kant het signaal detecteren als PTT trigger.

**Verworpen:** Opus narrowband (8 kHz samplerate) vernietigt:
- DC offset (highpass filter in Opus)
- Ultrasone tonen (Nyquist = 4 kHz)
- Pilottoon binnen de band zou hoorbaar zijn in de transmissie

Maar het **concept** klopt: PTT timing synchroniseren met audio timing door hetzelfde pad te gebruiken.

## Gekozen aanpak: jitter-buffer-output trigger

De PTT flag zit al als metadata in elk audio frame en reist door dezelfde jitter buffer als de audio. We gebruiken het moment dat het eerste PTT=true frame de jitter buffer **verlaat** als trigger voor `ZZTX1;`.

### Principe

```
Packet aankomst:  audio + PTT flag → push in jitter buffer
                  (NIET direct ZZTX1; sturen)

Playout tick:     pull frame uit jitter buffer
                  ├── frame heeft PTT=true EN state != Tx?
                  │   └── NU ZZTX1; sturen + audio naar cpal schrijven
                  └── frame heeft PTT=false EN state == Tx?
                      └── start tail delay (dynamisch)
```

### Wat dit elimineert

| Onzekerheidsbron | Vaste head delay (80ms) | Jitter-buffer trigger |
|---|---|---|
| Jitter buffer fill (40-60ms) | Mismatch mogelijk | **Geëlimineerd** |
| Playout tick fase (0-20ms) | Ongecompenseerd | **Geëlimineerd** |
| cpal + Thetis buffers (20-40ms) | Ongecompenseerd | Blijft (onvermijdelijk) |
| **Totale onzekerheid** | **±20ms** | **~0ms** (restant is consistent) |

Na de trigger:
- `ZZTX1;` gaat via CAT TCP naar Thetis: ~0ms (localhost)
- Audio gaat via cpal → VB-Cable → Thetis: ~20-40ms

Audio is altijd ~20-40ms **na** PTT, maar dit is **consistent** en klein genoeg — de TX schakelcircuit in de radio is niet instant, dus Thetis heeft toch een paar ms nodig om over te schakelen.

## Implementatieplan

### Server: ptt.rs

1. **Verwijder** `PTT_HEAD_MS` constante en `pending_activate` veld
2. **Nieuwe methode** `activate_from_playout()`: wordt aangeroepen door network.rs wanneer een PTT=true frame uit de jitter buffer komt. Stuurt `ZZTX1;` als state nog Rx is.
3. **Tail delay dynamisch maken**: bij PTT release, tail delay = `max(jitter_buf.depth() × 20, 80) + 40` ms (minimum 80ms + 40ms marge voor cpal/VB-Cable)

### Server: network.rs

4. **Playout loop aanpassen**: na `jitter_buf.pull()` → check PTT flag van het frame → als PTT=true en server niet in TX: roep `ptt.activate_from_playout()` aan
5. **PTT flag bewaren in frames**: de PTT flag zit al in `AudioPacket.flags`, maar wordt niet meegestuurd in de jitter buffer `BufferedFrame`. Toevoegen.
6. **Bij PTT release**: tail delay berekenen op basis van `jitter_buf.depth()`

### Client: engine.rs

7. **Audio gating** (al geïmplementeerd): alleen audio sturen als PTT actief is. Mic capture + AGC draaien altijd door.

### Geen wijzigingen nodig in:
- Protocol (protocol.rs) — PTT flag zit al in AudioPacket
- Client UI (ui.rs, MainScreen.kt) — geen impact
- CAT (cat.rs) — geen impact

## Verwacht resultaat

- **TX start**: geen ruisburst meer. Audio begint te stromen op VB-Cable ~20-40ms vóór Thetis naar TX schakelt (consistent)
- **TX release**: audio speelt volledig uit voordat Thetis terug naar RX gaat (dynamisch, past mee met netwerkconditie)
- **Bandbreedte besparing**: client stuurt geen audio meer als PTT uit is
