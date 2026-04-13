# ThetisLink v0.5.0 Implementatie

Gedetailleerde beschrijving van de implementatie per module, inclusief datastromen en algoritmen.

## sdr-remote-core

### protocol.rs — Pakketdefinities (~900 LOC)

Alle pakkettypen als Rust structs met `serialize()` / `deserialize()` methoden.

**Wire format voorbeeld — AudioPacket:**
```
Offset  Bytes  Veld
0       1      magic (0xAA)
1       1      version (0x01)
2       1      packet_type (0x01)
3       1      flags (bit 0 = PTT)
4       4      sequence (u32 LE)
8       4      timestamp (u32 LE, ms)
12      2      opus_len (u16 LE)
14      N      opus_data
```

#### PacketType enum (0x01–0x32)

| ID | Naam | Richting | Beschrijving |
|----|------|----------|-------------|
| 0x01 | Audio | bidi | RX1 audio (Opus narrowband/wideband) |
| 0x02 | Heartbeat | client→server | Heartbeat met timestamp |
| 0x03 | HeartbeatAck | server→client | Echo voor RTT meting |
| 0x04 | Control | bidi | ControlId + u16 waarde |
| 0x05 | Disconnect | client→server | Verbinding verbreken |
| 0x06 | PttDenied | server→client | TX geweigerd (andere client zendt) |
| 0x07 | Frequency | bidi | VFO-A frequentie (u64 Hz) |
| 0x08 | Mode | bidi | Operatiemode (u8) |
| 0x09 | Smeter | server→client | RX1 S-meter (u16 raw) |
| 0x0A | Spectrum | server→client | Spectrum view (zoom/pan) |
| 0x0B | FullSpectrum | server→client | Volledige DDC spectrum (waterval) |
| 0x0C | EquipmentStatus | server→client | Apparaatstatus (CSV labels) |
| 0x0D | EquipmentCommand | client→server | Apparaatcommando |
| 0x0E | AudioRx2 | server→client | RX2 audio (zelfde format als Audio) |
| 0x0F | FrequencyRx2 | bidi | VFO-B frequentie |
| 0x10 | ModeRx2 | bidi | RX2 mode |
| 0x11 | SmeterRx2 | server→client | RX2 S-meter |
| 0x12 | SpectrumRx2 | server→client | RX2 spectrum view |
| 0x13 | FullSpectrumRx2 | server→client | RX2 volledige DDC spectrum |
| 0x14 | Spot | server→client | DX cluster spot |
| 0x15 | TxProfiles | server→client | TX profielnamen |
| 0x16 | AudioYaesu | server→client | Yaesu audio (zelfde format als Audio) |
| 0x17 | YaesuState | server→client | Yaesu radiostatus |
| 0x18 | FrequencyYaesu | client→server | Yaesu frequentie instellen |
| 0x19 | YaesuMemoryData | server→client | Yaesu geheugendata (tab-separated) |
| 0x30 | AuthChallenge | server→client | 16-byte nonce |
| 0x31 | AuthResponse | client→server | 32-byte HMAC |
| 0x32 | AuthResult | server→client | 0=rejected, 1=accepted |

#### ControlId enum (0x01–0x46)

Elk Control pakket bevat een ControlId (u8) en een waarde (u16). Bidirectioneel: server stuurt huidige Thetis-waarden, client stuurt wijzigingen.

**Thetis basis (0x01–0x0D):**

| ID | Naam | Waarde | TCI/CAT |
|----|------|--------|---------|
| 0x01 | Rx1AfGain | 0-100 | ZZLA |
| 0x02 | PowerOnOff | 0/1 | ZZPS |
| 0x03 | TxProfile | 0-99 | ZZTP |
| 0x04 | NoiseReduction | 0-4 (0=off, 1-4=NR1-NR4) | ZZNE |
| 0x05 | AutoNotchFilter | 0/1 | ZZNT |
| 0x06 | DriveLevel | 0-100 | ZZPC |
| 0x07 | SpectrumEnable | 0/1 | — |
| 0x08 | SpectrumFps | 5-30 | — |
| 0x09 | SpectrumZoom | zoom x10 (10=1x, 10240=1024x) | — |
| 0x0A | SpectrumPan | (pan+0.5) x10000 (5000=center) | — |
| 0x0B | FilterLow | Hz offset (i16 als u16) | — |
| 0x0C | FilterHigh | Hz offset (i16 als u16) | — |
| 0x0D | ThetisStarting | 0/1 | — |

**RX2 / VFO-B (0x0E–0x1B):**

| ID | Naam | Waarde | TCI/CAT |
|----|------|--------|---------|
| 0x0E | Rx2Enable | 0/1 | — |
| 0x0F | Rx2AfGain | 0-100 | ZZLB |
| 0x10 | Rx2SpectrumZoom | zelfde als SpectrumZoom | — |
| 0x11 | Rx2SpectrumPan | zelfde als SpectrumPan | — |
| 0x12 | Rx2FilterLow | Hz offset (i16 als u16) | — |
| 0x13 | Rx2FilterHigh | Hz offset (i16 als u16) | — |
| 0x14 | VfoSync | 0/1 (VFO-B volgt VFO-A) | — |
| 0x15 | Rx2SpectrumEnable | 0/1 | — |
| 0x16 | Rx2SpectrumFps | 5-30 | — |
| 0x17 | Rx2NoiseReduction | 0-4 | — |
| 0x18 | Rx2AutoNotchFilter | 0/1 | — |
| 0x19 | VfoSwap | write-only trigger (ZZVS2) | — |
| 0x1A | SpectrumMaxBins | max bins/pakket (0=default) | — |
| 0x1B | Rx2SpectrumMaxBins | zelfde als SpectrumMaxBins | — |

**Spectrum configuratie (0x1C–0x1D):**

| ID | Naam | Waarde |
|----|------|--------|
| 0x1C | SpectrumFftSize | grootte in K (32, 65, 131, 262) |
| 0x1D | SpectrumBinDepth | 8=u8 bins, 16=u16 bins |

**Thetis uitgebreid (0x1E–0x1F):**

| ID | Naam | Waarde | TCI/CAT |
|----|------|--------|---------|
| 0x1E | MonitorOn | 0/1 | ZZMO / TCI: MON_ENABLE |
| 0x1F | ThetisTune | 0/1 | ZZTU |

**Yaesu FT-991A (0x20–0x2F):**

| ID | Naam | Waarde |
|----|------|--------|
| 0x20 | YaesuEnable | 0/1 (stream audio+state) |
| 0x21 | YaesuPtt | 0/1 |
| 0x22 | YaesuFreq | (via FrequencyPacket) |
| 0x23 | YaesuMicGain | gain x10 (200=20.0x) |
| 0x24 | YaesuMode | intern modenummer |
| 0x25 | YaesuReadMemories | trigger |
| 0x26 | YaesuRecallMemory | kanaal 1-99 |
| 0x27 | YaesuWriteMemories | trigger |
| 0x28 | YaesuSelectVfo | 0=A, 1=B, 2=swap |
| 0x29 | YaesuSquelch | 0-255 |
| 0x2A | YaesuRfGain | 0-255 |
| 0x2B | YaesuRadioMicGain | 0-100 (radio mic gain) |
| 0x2C | YaesuRfPower | 0-100 |
| 0x2D | YaesuButton | button ID |
| 0x2E | YaesuReadMenus | trigger |
| 0x2F | YaesuSetMenu | menunummer (P2 in apart pakket) |

**TCI controls (0x30–0x3C):**

| ID | Naam | Waarde | TCI |
|----|------|--------|-----|
| 0x30 | AgcMode | 0=off, 1=long, 2=slow, 3=med, 4=fast, 5=custom | agc_mode |
| 0x31 | AgcGain | 0-120 | agc_gain |
| 0x32 | RitEnable | 0/1 | rit_enable |
| 0x33 | RitOffset | Hz (i16 als u16) | rit_offset |
| 0x34 | XitEnable | 0/1 | xit_enable |
| 0x35 | XitOffset | Hz (i16 als u16) | xit_offset |
| 0x36 | SqlEnable | 0/1 | sql_enable |
| 0x37 | SqlLevel | 0-160 | sql_level |
| 0x38 | NoiseBlanker | 0/1 | rx_nb_enable |
| 0x39 | CwKeyerSpeed | 1-60 WPM | cw_keyer_speed |
| 0x3A | VfoLock | 0/1 | vfo_lock |
| 0x3B | Binaural | 0/1 | rx_bin_enable |
| 0x3C | ApfEnable | 0/1 | rx_apf_enable |

**Diversity (0x40–0x46):**

| ID | Naam | Waarde |
|----|------|--------|
| 0x40 | DiversityEnable | 0/1 |
| 0x41 | DiversityRef | 0=RX2, 1=RX1 |
| 0x42 | DiversitySource | 0=RX1+RX2, 1=RX1, 2=RX2 |
| 0x43 | DiversityGainRx1 | gain x1000 (2500=2.500) |
| 0x44 | DiversityGainRx2 | gain x1000 (2500=2.500) |
| 0x45 | DiversityPhase | phase x100 + 18000 (18000=0 graden) |
| 0x46 | DiversityRead | trigger (lees state uit Thetis) |

**EquipmentStatus/Command:** Variabele lengte, CSV-gecodeerde telemetrie in een `labels` string veld. Elke apparaattype heeft een eigen CSV-layout.

### codec.rs — Opus Audio Codec (~230 LOC)

Twee codec configuraties: narrowband voor RX/TX audio en wideband voor Thetis TX en Yaesu audio.

| Parameter | Narrowband | Wideband |
|-----------|-----------|----------|
| Sample rate | 8 kHz | 16 kHz |
| Bitrate | 12.8 kbps | 24 kbps |
| Frame size | 160 samples (20ms) | 320 samples (20ms) |
| Bandwidth | Narrowband | Wideband |
| FEC | Inband, 10% loss | Inband, 10% loss |
| DTX | Aan | Aan |
| Signaaltype | Voice | Voice |

**Belangrijk:** Narrowband bitrate 12.8 kbps ligt net boven de Opus FEC drempel (12.4 kbps). Dit garandeert dat Forward Error Correction altijd meegestuurd wordt.

**Klassen:**
- `OpusEncoder` / `OpusDecoder` — 8 kHz narrowband (RX1/RX2 audio)
- `OpusEncoderWideband` / `OpusDecoderWideband` — 16 kHz wideband (Thetis TX, Yaesu audio)

**Decode methoden (OpusDecoder):**
- `decode(opus_data)` — normaal decoderen, retourneert 160 i16 samples
- `decode_fec(next_opus_data)` — FEC recovery met data van het *volgende* pakket
- `decode_plc()` — Packet Loss Concealment (comfort noise/interpolatie) als geen data beschikbaar

**Constanten (lib.rs):**
- `NETWORK_SAMPLE_RATE` = 8000 Hz (narrowband)
- `NETWORK_SAMPLE_RATE_WIDEBAND` = 16000 Hz (wideband)
- `DEVICE_SAMPLE_RATE` = 48000 Hz (cpal/WASAPI/Oboe)
- `FRAME_DURATION_MS` = 20 ms
- `FRAME_SAMPLES` = 160 (8kHz x 20ms)
- `FRAME_SAMPLES_WIDEBAND` = 320 (16kHz x 20ms)

### jitter.rs — Adaptieve Jitter Buffer (~350 LOC)

```mermaid
graph TD
    A[UDP Pakket Ontvangen] --> B{Sequence < next_seq?}
    B -->|Ja: te laat| C[Drop pakket]
    B -->|Nee| D[Bereken jitter<br/>RFC 3550 dual-alpha]
    D --> E[Voeg toe aan BTreeMap]
    E --> F{Buffer > target+6?}
    F -->|Ja| G[Overflow recovery:<br/>drop 1-2 frames]
    F -->|Nee| H[Klaar]

    I[Pull 20ms tick] --> J{Frame beschikbaar<br/>voor next_seq?}
    J -->|Ja| K[Return Frame<br/>next_seq++]
    J -->|Nee| L{Volgende seq<br/>in buffer?}
    L -->|Ja| M[Return Missing<br/>gebruik FEC van volgende]
    L -->|Nee| N{Buffer leeg?}
    N -->|Ja| O[Return NotReady<br/>gebruik PLC]
    N -->|Nee| P{Ver achter?<br/>gap > max_depth}
    P -->|Ja| Q[Skip-ahead naar<br/>oudste frame]
    P -->|Nee| O
```

**Jitter schatting (RFC 3550 dual-alpha EMA):**
```
deviation = |verwachte_interval - werkelijke_interval|

als deviation > huidige_schatting:
    jitter = jitter + 0.25 * (deviation - jitter)     # snelle attack
anders:
    jitter = jitter + 0.0625 * (deviation - jitter)   # trage decay
```

De dual-alpha aanpak (alpha=0.25 bij stijging, alpha=1/16 bij daling) zorgt ervoor dat de buffer snel reageert op verslechtering (jitter spikes) maar langzaam krimpt bij verbetering. Dit voorkomt dat de buffer oscilleert.

**Spike peak hold:** Piekwaarde met exponentieel verval (~1 minuut bij 50 pakketten/sec, decay factor = 1 - 1/3000). Voorkomt dat buffer te snel krimpt na een netwerkpiek.

**Target depth formule:**
```
target = max(jitter_estimate, spike_peak) / 15.0 + 2
clamped: 2..40 frames (40ms..800ms)
```

**Grace period:** Eerste 25 pulls (500ms) na verbinding: geen overflow recovery. Laat buffer stabiliseren.

**Underflow recovery:** Wanneer de buffer volledig leeg raakt (`refilling = true`), pauzeert de playout tot de buffer weer op target depth is. Dit voorkomt stuttering bij korte netwerkonderbrekingen.

**JitterResult enum:**
- `Frame(BufferedFrame)` — normaal frame beschikbaar
- `Missing` — frame mist, caller gebruikt FEC of PLC
- `NotReady` — buffer nog niet gevuld of leeg

## sdr-remote-logic

### commands.rs — Command Enum (~106 varianten)

Commands worden via `mpsc::UnboundedSender<Command>` van UI naar engine gestuurd.

| Groep | Commands | Aantal |
|-------|---------|--------|
| Verbinding | `Connect(addr, password)`, `Disconnect` | 2 |
| Audio | `SetRxVolume`, `SetLocalVolume`, `SetVfoAVolume`, `SetVfoBVolume`, `SetTxGain`, `SetInputDevice`, `SetOutputDevice` | 7 |
| Radio | `SetPtt`, `SetFrequency`, `SetMode`, `SetControl`, `SetAgcEnabled` | 5 |
| Spectrum | `EnableSpectrum`, `SetSpectrumFps`, `SetSpectrumZoom`, `SetSpectrumPan`, `SetSpectrumMaxBins`, `SetSpectrumFftSize` | 6 |
| RX2/VFO-B | `SetRx2Enabled`, `SetVfoSync`, `SetFrequencyRx2`, `SetModeRx2`, `SetRx2Volume`, `EnableRx2Spectrum`, `SetRx2SpectrumFps`, `SetRx2SpectrumZoom`, `SetRx2SpectrumPan` | 9 |
| Thetis | `ThetisTune(bool)`, `SetMonitor(bool)` | 2 |
| Amplitec | `SetAmplitecSwitchA(u8)`, `SetAmplitecSwitchB(u8)` | 2 |
| Tuner | `TunerTune`, `TunerAbort` | 2 |
| SPE Expert | `SpeOperate`, `SpeTune`, `SpeAntenna`, `SpeInput`, `SpePower`, `SpeBandUp`, `SpeBandDown`, `SpeOff`, `SpePowerOn`, `SpeDriveDown`, `SpeDriveUp` | 11 |
| RF2K-S | `Rf2kOperate(bool)`, `Rf2kTune`, `Rf2kAnt1`–`Rf2kAnt4`, `Rf2kAntExt`, `Rf2kErrorReset`, `Rf2kClose`, `Rf2kDriveUp/Down`, `Rf2kTunerMode/Bypass/Reset/Store/LUp/LDown/CUp/CDown/K`, `Rf2kSetHighPower/Tuner6m/BandGap`, `Rf2kFrqDelayUp/Down`, `Rf2kAutotuneThresholdUp/Down`, `Rf2kDacAlcUp/Down`, `Rf2kZeroFRAM`, `Rf2kSetDriveConfig` | 30 |
| UltraBeam | `UbRetract`, `UbSetFrequency(khz, direction)`, `UbReadElements` | 3 |
| Rotor | `RotorGoTo(angle_x10)`, `RotorStop`, `RotorCw`, `RotorCcw` | 4 |
| Yaesu | `SetYaesuVolume`, `SetYaesuPtt`, `SetYaesuFreq`, `SetYaesuMode`, `SetYaesuMenu`, `WriteYaesuMemories`, `SetYaesuTxGain` | 7 |
| Server | `ServerReboot` | 1 |

### state.rs — RadioState (~250 velden)

Broadcast van engine naar UI via `watch::Sender<RadioState>`. UI ontvangt via `watch::Receiver` met change notification.

**Verbinding & statistieken:**
- `connected`, `ptt_denied`, `audio_error`
- `rtt_ms`, `jitter_ms`, `buffer_depth`, `rx_packets`, `loss_percent`

**Audio niveaus:**
- `capture_level`, `playback_level`, `playback_level_rx2`
- `playback_level_yaesu`, `yaesu_mic_level`

**Radio (Thetis RX1):**
- `frequency_hz`, `mode`, `smeter`
- `power_on`, `tx_profile`, `nr_level`, `anf_on`, `drive_level`
- `rx_af_gain`, `agc_enabled`, `other_tx`
- `filter_low_hz`, `filter_high_hz`
- `thetis_starting`, `mon_on`
- `tx_profile_names: Vec<String>`

**RX2 / VFO-B:**
- `rx2_enabled`, `vfo_sync`
- `frequency_rx2_hz`, `mode_rx2`, `smeter_rx2`
- `rx2_af_gain`, `rx2_nr_level`, `rx2_anf_on`
- `filter_rx2_low_hz`, `filter_rx2_high_hz`

**TCI controls (v0.5.0):**
- `agc_mode` (0=off, 1=long, 2=slow, 3=med, 4=fast, 5=custom)
- `agc_gain` (0-120)
- `rit_enable`, `rit_offset` (Hz)
- `xit_enable`, `xit_offset` (Hz)
- `sql_enable`, `sql_level` (0-160)
- `nb_enable`
- `cw_keyer_speed` (WPM)
- `vfo_lock`, `binaural`, `apf_enable`

**Diversity:**
- `diversity_enabled`, `diversity_ref`, `diversity_source`
- `diversity_gain_rx1`, `diversity_gain_rx2` (x1000)
- `diversity_phase` (phase x100 + 18000)

**Spectrum (RX1 + RX2):**
- `spectrum_bins: Vec<u16>`, `spectrum_center_hz`, `spectrum_span_hz`
- `spectrum_ref_level`, `spectrum_db_per_unit`, `spectrum_sequence`
- `full_spectrum_bins`, `full_spectrum_center_hz`, `full_spectrum_span_hz`, `full_spectrum_sequence`
- (Identieke set voor RX2 met `rx2_` prefix)

**Externe apparatuur — Amplitec 6/2:**
- `amplitec_connected`, `amplitec_switch_a/b` (0=unknown, 1-6), `amplitec_labels`

**JC-4s Antennetuner:**
- `tuner_connected`, `tuner_state` (0=Idle, 1=Tuning, 2=DoneOk, 3=Timeout, 4=Aborted), `tuner_can_tune`

**SPE Expert 1.3K-FA:**
- `spe_connected`, `spe_state` (0=Off, 1=Standby, 2=Operate)
- `spe_band`, `spe_ptt`, `spe_power_w`, `spe_swr_x10`, `spe_temp`
- `spe_warning`, `spe_alarm`, `spe_power_level`, `spe_antenna`, `spe_input`
- `spe_voltage_x10`, `spe_current_x10`, `spe_atu_bypassed`
- `spe_available`, `spe_active`

**RF2K-S Power Amplifier:**
- `rf2k_connected`, `rf2k_operate`, `rf2k_band`, `rf2k_frequency_khz`
- `rf2k_temperature_x10`, `rf2k_voltage_x10`, `rf2k_current_x10`
- `rf2k_forward_w`, `rf2k_reflected_w`, `rf2k_swr_x100`
- `rf2k_max_forward_w`, `rf2k_max_reflected_w`, `rf2k_max_swr_x100`
- `rf2k_error_state`, `rf2k_error_text`
- `rf2k_antenna_type`, `rf2k_antenna_number`, `rf2k_tuner_mode`, `rf2k_tuner_setup`
- `rf2k_tuner_l_nh`, `rf2k_tuner_c_pf`, `rf2k_tuner_freq_khz`, `rf2k_segment_size_khz`
- `rf2k_drive_w`, `rf2k_modulation`, `rf2k_max_power_w`, `rf2k_device_name`
- `rf2k_available`, `rf2k_active`
- Debug (Fase D): `rf2k_debug_available`, `rf2k_bias_pct_x10`, `rf2k_psu_source`, `rf2k_uptime_s`, `rf2k_tx_time_s`, `rf2k_error_count`, `rf2k_error_history`, `rf2k_storage_bank`, `rf2k_hw_revision`, `rf2k_frq_delay`, `rf2k_autotune_threshold_x10`, `rf2k_dac_alc`, `rf2k_high_power`, `rf2k_tuner_6m`, `rf2k_band_gap_allowed`, `rf2k_controller_version`, `rf2k_drive_config_ssb/am/cont`

**UltraBeam RCU-06:**
- `ub_connected`, `ub_frequency_khz`, `ub_band`, `ub_direction`
- `ub_off_state`, `ub_motors_moving`, `ub_motor_completion`
- `ub_fw_major`, `ub_fw_minor`, `ub_available`, `ub_elements_mm: [u16; 6]`

**DX Cluster:**
- `dx_spots: Vec<DxSpotInfo>` — callsign, frequency_hz, mode, spotter, comment, age/expiry

**EA7HG Visual Rotor:**
- `rotor_connected`, `rotor_angle_x10`, `rotor_rotating`, `rotor_target_x10`, `rotor_available`

**Yaesu FT-991A:**
- `yaesu_connected`, `yaesu_freq_a`, `yaesu_freq_b`, `yaesu_mode`, `yaesu_smeter`
- `yaesu_tx_active`, `yaesu_power_on`, `yaesu_af_gain`, `yaesu_tx_power`
- `yaesu_squelch`, `yaesu_rf_gain`, `yaesu_mic_gain`, `yaesu_split`
- `yaesu_vfo_select` (0=VFO, 1=Memory, 2=MemTune), `yaesu_memory_channel`
- `yaesu_memory_data: Option<String>`

**Authenticatie:**
- `auth_rejected`

### engine.rs — ClientEngine (~2.181 LOC)

De engine is het hart van elke client. Draait als async tokio task.

```mermaid
graph TB
    subgraph "Engine Main Loop (tokio::select!)"
        CMD[Command Channel<br/>mpsc::UnboundedReceiver]
        UDP[UDP Socket<br/>recv_from]
        TICK[Audio Tick<br/>20ms interval]
        HB[Heartbeat<br/>500ms interval]
    end

    subgraph "Command Processing"
        CMD --> CONN[Connect/Disconnect]
        CMD --> PTT[Set PTT]
        CMD --> VOL[Volume instellingen]
        CMD --> FREQ[Frequentie/Mode]
        CMD --> SPEC[Spectrum settings]
        CMD --> EQUIP[Equipment commands]
    end

    subgraph "UDP Ontvangst"
        UDP --> AUDIO_RX[Audio → Jitter Buffer]
        UDP --> HBACK[HeartbeatAck → RTT update]
        UDP --> STATE_UPD[Freq/Mode/Smeter → State]
        UDP --> SPEC_RX[Spectrum → State bins]
        UDP --> CTRL_RX[Control → State sync]
        UDP --> EQUIP_RX[EquipmentStatus → State]
    end

    subgraph "Audio Tick (20ms)"
        TICK --> PLAY[RX Playout]
        TICK --> CAP[TX Capture]
        PLAY --> JB1[RX1 Jitter Buffer Pull]
        PLAY --> JB2[RX2 Jitter Buffer Pull]
        JB1 --> DEC1[Opus Decode / FEC / PLC]
        JB2 --> DEC2[Opus Decode RX2]
        DEC1 --> RES1[Resample 8→48kHz]
        DEC2 --> RES2[Resample 8→48kHz]
        RES1 --> MIX[Mix RX1 + RX2]
        RES2 --> MIX
        MIX --> RING_OUT[Playback Ring Buffer]

        CAP --> RING_IN[Capture Ring Buffer]
        RING_IN --> RES_TX[Resample 48→8kHz]
        RES_TX --> AGC[TX AGC]
        AGC --> ENC[Opus Encode]
        ENC --> SEND[UDP Send]
    end

    subgraph "State Broadcast"
        STATE[RadioState via watch::send]
    end

    AUDIO_RX --> STATE
    STATE_UPD --> STATE
    PLAY --> STATE
    CAP --> STATE
```

#### Audio Playout (RX) — Detail

```mermaid
graph TD
    A[20ms Tick] --> B[Check Ring Buffer niveau]
    B --> C{Buffer < 60ms?}
    C -->|Ja| D[Pull 2 frames<br/>uit jitter buffer]
    C -->|Nee| E{Buffer > 200ms?}
    E -->|Ja| F[Skip pull<br/>laat buffer leeglopen]
    E -->|Nee| G[Pull 1 frame]

    D --> H{Frame beschikbaar?}
    G --> H
    H -->|Frame| I[Opus Decode<br/>160 samples i16]
    H -->|Missing| J[FEC Decode<br/>of PLC]
    H -->|NotReady| K[PLC<br/>comfort noise]

    I --> L[Resample 8→48kHz<br/>rubato SincFixedIn]
    J --> L
    K --> L

    L --> M[Apply Volume<br/>rx x vfoA x master]
    M --> N{RX2 enabled?}
    N -->|Ja| O[Pull RX2 frames<br/>match RX1 count]
    O --> P[Decode + Resample RX2]
    P --> Q[Apply Volume<br/>rx2 x vfoB x master]
    Q --> R[Mix: RX1 + RX2<br/>additief]
    N -->|Nee| R
    R --> S[Write naar<br/>Playback Ring Buffer]
```

#### Audio Capture (TX) — Detail

```mermaid
graph TD
    A[20ms Tick] --> B[Pull samples uit<br/>Capture Ring Buffer]
    B --> C[Resample 48→8kHz<br/>rubato SincFixedIn]
    C --> D[Apply TX Gain<br/>standaard 0.5]
    D --> E{AGC enabled?}
    E -->|Ja| F[TX AGC:<br/>target -12dB<br/>range +-20dB<br/>attack 0.3, release 0.01<br/>noise gate -60dB]
    E -->|Nee| G[Accumuleer in buffer]
    F --> G
    G --> H{160 samples?}
    H -->|Nee| I[Wacht op meer]
    H -->|Ja| J[Opus Encode<br/>160 smp → ~60 bytes]
    J --> K[Bouw AudioPacket<br/>seq++, timestamp]
    K --> L{PTT transitie?}
    L -->|Ja| M[Burst: 5 pakketten<br/>snel achter elkaar]
    L -->|Nee| N[Normaal verzenden]
    M --> O[UDP Send]
    N --> O
```

#### Frequentie Synchronisatie

```mermaid
sequenceDiagram
    participant UI as Client UI
    participant E as Engine
    participant S as Server
    participant T as Thetis (TCI)

    UI->>E: SetFrequency(7.035 MHz)
    E->>E: pending_freq = 7.035 MHz
    E->>S: FrequencyPacket(7.035 MHz)
    S->>T: TCI: vfo:0,0,7035000;

    Note over S: TCI notificatie
    T->>S: vfo:0,0,7035000;
    S->>E: FrequencyPacket(7.035 MHz)

    E->>E: ontvangen == pending?
    Note over E: Ja → pending_freq = None
    E->>E: state.frequency_hz = 7.035 MHz

    Note over T: Gebruiker draait aan VFO in Thetis
    T->>S: TCI: vfo:0,0,7036000;
    S->>E: FrequencyPacket(7.036 MHz)
    E->>E: pending == None → accepteer
    E->>E: state.frequency_hz = 7.036 MHz
```

#### Volume Synchronisatie

```mermaid
graph TD
    A[Client start] --> B[rx_volume_synced = false]
    B --> C{Server stuurt<br/>Rx1AfGain control?}
    C -->|Ja| D[rx_volume_synced = true<br/>rx_volume = server waarde]
    C -->|Nee| E[Wacht...]

    D --> F{Gebruiker past<br/>RX volume aan?}
    F -->|Ja| G[Stuur SetRxVolume<br/>naar engine]
    G --> H[Engine stuurt<br/>Control naar server]
    H --> I[Server stuurt<br/>TCI: rx_volume naar Thetis]
```

## sdr-remote-server

### TCI als enige verbindingsmodus

De server verbindt uitsluitend via TCI (Thetis Control Interface) met Thetis. De TCI WebSocket verbinding verzorgt:
- RX audio (IQ of demodulated)
- Frequentie, mode, S-meter en alle radio controls
- Spectrum data (indien ingeschakeld)
- TX audio
- PTT, volume, en alle andere controls

### Hoofdstructuur

```mermaid
graph TB
    subgraph "main.rs — Opstart"
        CONF[Config laden/GUI]
        TCI_INIT[TCI verbinding<br/>WebSocket naar Thetis]
        HPSDR[HPSDR Capture<br/>DDC I/Q listener]
        EQUIP[Equipment Controllers<br/>COM/TCP/HTTP]
        NET[NetworkService starten]
    end

    CONF --> TCI_INIT
    CONF --> HPSDR
    CONF --> EQUIP
    TCI_INIT --> NET
    HPSDR --> NET
    EQUIP --> NET
```

### network.rs — NetworkService (~1.363 LOC)

Beheert alle UDP communicatie met clients.

```mermaid
graph TB
    subgraph "Async Tasks"
        HB[Heartbeat Responder<br/>500ms interval]
        TX[Audio TX Task<br/>Capture → Encode → Send]
        RX[Audio RX Task<br/>Recv → Decode → TCI TX]
        SPEC[Spectrum Task<br/>FFT → View Extract → Send]
        SPEC2[RX2 Spectrum Task]
        TCI_SYNC[TCI State Sync<br/>Event-driven]
        CTRL[Control Broadcast<br/>State sync naar clients]
    end

    subgraph "Gedeelde State"
        SESSION[SessionManager<br/>HashMap addr → ClientSession]
        PTT_CTRL[PttController<br/>Single-TX arbitrage]
        TCI_STATE[TCI State<br/>Freq, Mode, Meters]
    end

    HB --> SESSION
    TX --> SESSION
    RX --> SESSION
    RX --> PTT_CTRL
    SPEC --> SESSION
    TCI_SYNC --> TCI_STATE
    CTRL --> SESSION
```

### Two-Phase Connect & Lock Contention Fix

De verbinding met Thetis TCI WebSocket gebruikt een two-phase connect patroon:

1. **Phase 1 — needs_connect_info():** De TCI module signaleert dat een (her)verbinding nodig is.
2. **Phase 2 — accept_stream():** De caller (network.rs) maakt de TCP/WebSocket verbinding in een achtergrond tokio task met timeout (500ms), en geeft de verbonden stream door.

Dit voorkomt dat een blokkerende connect de main loop ophoudt.

**Lock contention fix:** De drie TCI consumer tasks (audio, spectrum, control) delen een `Mutex<TciClient>`. Om contention te voorkomen:
- `drop(ptt_guard)` wordt aangeroepen voor elke `sleep` in de tasks
- Connect timeouts: 100ms TCP, 500ms WebSocket
- Reconnect interval: 1 seconde

### tci.rs — TCI Interface

TCI (Thetis Control Interface) is een WebSocket-gebaseerd protocol voor bidirectionele communicatie met Thetis SDR. In tegenstelling tot polling ontvangt TCI event-driven updates.

**TCI biedt:**
- Real-time frequentie/mode/S-meter notificaties (geen polling nodig)
- Audio streaming (RX en TX) via het WebSocket protocol
- Directe controle van alle radio parameters
- Spectrum data

**State sync:** TCI stuurt automatisch updates wanneer waarden veranderen in Thetis, wat lagere latency geeft dan polling-gebaseerde alternatieven.

### spectrum.rs — SpectrumProcessor (~994 LOC)

**DDC FFT Pipeline:**

```mermaid
graph TD
    A[ANAN 7000DLE<br/>DDC I/Q samples] --> B[HPSDR Capture<br/>UDP :1037]
    B --> C[Accumuleer complex samples]
    C --> D{fft_size/2<br/>nieuwe samples?}
    D -->|Nee| C
    D -->|Ja| E[50% overlap:<br/>combineer met vorige helft]
    E --> F[Hann Window toepassen]
    F --> G[Complex FFT<br/>bijv. 262144-punt]
    G --> H[Magnitude berekenen<br/>20 x log10]
    H --> I[EMA Smoothing<br/>alpha = 0.1]
    I --> J[8192 bins output]

    J --> K{Per client}
    K --> L[View extractie:<br/>zoom & pan toepassen]
    L --> M[Downsample naar<br/>gevraagd aantal bins]
    M --> N[SpectrumPacket verzenden]
```

**FFT grootte selectie:**
```
target = sample_rate / 6
fft_size = next_power_of_two(target)
minimum = 4096

Voorbeelden:
  1536 kHz → 262144 (~12 FPS)
   384 kHz →  65536 (~12 FPS)
    96 kHz →  16384 (~12 FPS)
    48 kHz →   8192 (~12 FPS)
```

### ptt.rs — PTT Controller (~559 LOC)

**Single-TX Arbitrage:**

```mermaid
stateDiagram-v2
    [*] --> Idle: Start
    Idle --> TxActive: Client A PTT=1<br/>try_acquire_tx(A) → true
    TxActive --> Idle: Client A PTT=0<br/>release_tx(A)
    TxActive --> TxActive: Client B PTT=1<br/>try_acquire_tx(B) → false<br/>→ PttDenied

    state TxActive {
        [*] --> HoldingTx
        HoldingTx: tx_holder = Client A
        HoldingTx: TCI: trx 0,true;
    }

    state Idle {
        [*] --> NoTx
        NoTx: tx_holder = None
        NoTx: TCI: trx 0,false;
    }
```

### Equipment Handlers

Alle apparaathandlers volgen hetzelfde patroon:

```mermaid
graph TD
    A[Init: open COM/TCP/HTTP] --> B[Polling Loop]
    B --> C[Status uitlezen]
    C --> D[CSV encoderen in<br/>EquipmentStatus labels]
    D --> E[Broadcast naar clients]
    E --> B

    F[EquipmentCommand<br/>van client] --> G[Parse command type]
    G --> H[Vertaal naar<br/>apparaat protocol]
    H --> I[Verstuur naar apparaat]
```

| Handler | Interface | Poll Interval | Telemetrie Velden |
|---------|-----------|---------------|-------------------|
| amplitec.rs (220 LOC) | COM 9600 | 1s | switch_a, switch_b, labels |
| tuner.rs (503 LOC) | COM 9600 | 500ms | state, can_tune |
| spe_expert.rs (568 LOC) | COM 9600 | 500ms | 12 velden (power, SWR, temp, ...) |
| rf2k.rs (1082 LOC) | HTTP :8080 | 500ms | 28+ velden incl. debug |
| ultrabeam.rs (461 LOC) | COM 9600 | 1s | freq, band, direction, elements |
| rotor.rs (245 LOC) | TCP :3010 | 500ms | angle, rotating, target |

### Yaesu FT-991A Integratie

De server ondersteunt een optionele Yaesu FT-991A radio naast de Thetis/ANAN. De Yaesu wordt via een serieel CAT-protocol aangestuurd (apart van de TCI verbinding met Thetis).

**Functionaliteit:**
- Aparte audio stream (AudioYaesu, 0x16) via wideband Opus (16 kHz)
- Eigen frequentie/mode/S-meter in YaesuState pakketten
- Geheugenkanaal lezen/schrijven (YaesuMemoryData, tab-separated)
- VFO-A/B selectie en swap
- Squelch, RF gain, mic gain, RF power bediening
- EX menu lezen en schrijven
- Knoppen (button ID's) voor functies zonder eigen ControlId

**Audio:** De Yaesu audio loopt via een apart codec pad met wideband Opus (16 kHz, 24 kbps) voor betere audiokwaliteit.

### Diversity Control

Diversity combining maakt gebruik van twee ontvangantennes via RX1 en RX2 van de ANAN 7000DLE.

**Controls (via ControlId 0x40–0x46):**
- Enable/disable diversity mode
- Selectie referentiebron (RX1 of RX2)
- Selectie luisterbron (RX1+RX2 combined, RX1 only, RX2 only)
- Gain per ontvanger (0.000 tot 65.535, resolutie 0.001)
- Fase-instelling (-180.00 tot +180.00 graden, resolutie 0.01)
- Read trigger om huidige state uit Thetis te laden

## sdr-remote-client

### main.rs — Opstart

```mermaid
graph TD
    A[Start] --> B[Init tokio runtime]
    B --> C[Maak ClientAudio<br/>cpal devices]
    C --> D[Maak ClientEngine<br/>uit sdr-remote-logic]
    D --> E[Spawn engine<br/>in achtergrond]
    E --> F[Start eframe/egui<br/>rendering loop]
    F --> G[UI update() per frame]
```

### audio.rs — ClientAudio

```mermaid
graph LR
    subgraph "Input (Capture)"
        MIC[Microfoon] --> CPAL_IN[cpal Input Stream]
        CPAL_IN --> RING_IN[Ring Buffer<br/>lock-free SPSC]
    end

    subgraph "Output (Playback)"
        RING_OUT[Ring Buffer<br/>lock-free SPSC] --> CPAL_OUT[cpal Output Stream]
        CPAL_OUT --> SPK[Luidsprekers]
    end

    RING_IN -.->|Engine leest| ENGINE[Engine]
    ENGINE -.->|Engine schrijft| RING_OUT
```

### ui.rs — Desktop UI (~5.668 LOC)

Zie apart document: [UI.md](UI.md)

## sdr-remote-android

### Architectuur

```mermaid
graph TB
    subgraph "Kotlin / Jetpack Compose"
        Activity[MainActivity]
        VM[SdrViewModel]
        MS[MainScreen]
        RC[RadioControls]
        FD[FrequencyDisplay]
        SV[SpectrumView]
        SP[StatsPanel / VolumeControls]
    end

    subgraph "Rust via JNI"
        Bridge[bridge.rs<br/>FFI interface]
        Engine[ClientEngine<br/>sdr-remote-logic]
        Core[sdr-remote-core<br/>Protocol + Codec]
    end

    Activity --> VM
    VM --> Bridge
    MS --> RC
    MS --> FD
    MS --> SV
    MS --> SP

    Bridge --> Engine
    Engine --> Core

    VM -.->|state polling| Bridge
    VM -.->|commands| Bridge
```

**Bridge functies (Rust → Kotlin):**
- `version()` → String
- `state()` → BridgeRadioState (250+ velden)
- `connect(addr, password)`, `disconnect()`
- `set_ptt(bool)`, `set_frequency(hz)`, `set_mode(u8)`
- `set_rx_volume(f32)`, `set_local_volume(f32)`, `set_tx_gain(f32)`
- `set_control(id, value)`
- `enable_spectrum(bool)`, `set_spectrum_fps/zoom/pan()`

**Audio:** Oboe (Android Native Audio), 48kHz mono f32

## Netwerk Timing & Betrouwbaarheid

### Tijdslijn van een audio frame

```
t=0ms    Client capture ring buffer → samples beschikbaar
t=1ms    Resample 48→8kHz, Opus encode
t=2ms    UDP verzenden
t=Xms    Netwerk transit (RTT/2)
t=X+1ms  Server ontvangst
t=X+2ms  Opus decode, resample 8→48kHz
t=X+3ms  Via TCI naar Thetis TX
```

Totale one-way latency: ~3ms processing + netwerk transit + jitter buffer (40-800ms adaptief)

### Heartbeat & Verbindingsdetectie

```
Interval:     500ms
Timeout:      max(6000ms, RTT x 8)
RTT meting:   Echo timestamp in HeartbeatAck
Loss%:        Rolling window per heartbeat interval
Reconnect:    Reset codec + jitter buffer bij eerste HeartbeatAck
```

### Pakketverlies Herstel

| Scenario | Herstel Methode |
|----------|----------------|
| 1 pakket verloren | FEC uit volgend pakket |
| 2+ pakketten verloren | PLC (Packet Loss Concealment) |
| Burst verlies | Jitter buffer absorbeert tot target depth |
| Netwerk piek | Spike peak hold voorkomt te snelle buffer krimp |
| Verbinding weg | Timeout na 6s, reconnect bij nieuwe HeartbeatAck |

## Authenticatie

Optionele wachtwoordbeveiliging via HMAC-SHA256 challenge-response:

1. Server stuurt `AuthChallenge` met 16-byte random nonce
2. Client berekent HMAC-SHA256(nonce, wachtwoord) en stuurt `AuthResponse`
3. Server verifieert en stuurt `AuthResult` (0=rejected, 1=accepted)

Bij foutief wachtwoord: `state.auth_rejected = true`, verbinding wordt verbroken.

## PowerOnOff & State Sync

Power on/off logica met race condition preventie:

- **Lokale state:** `value == 1` voor correcte toggle
- **state_tx.send()** direct na PowerOnOff voor onmiddellijke UI update
- **power_suppress_until:** 5 seconden onderdrukking van server power broadcasts na lokale toggle, voorkomt dat server state de lokale wijziging terugdraait
