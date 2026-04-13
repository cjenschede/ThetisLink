# TCI Commands — Missing or Not Working in Thetis

We're building a remote control app (ThetisLink) for the ANAN 7000DLE using TCI as the primary interface. Most TCI commands work great — VFO, DDS, modulation, drive, filters, PTT (TRX), audio streams, IQ streams, START/STOP all work perfectly.

Below is a list of TCI commands that either don't work as expected or have no TCI equivalent, requiring a fallback to TCP CAT.

## Not working / not implemented

| TCI Command | Purpose | Expected behavior | Current workaround |
|---|---|---|---|
| `RX_VOLUME:0,0,db;` | Set RX1 AF gain | Should control RX1 audio volume (-60..0 dB) | TCP CAT `ZZLA` |
| `RX_VOLUME:0,1,db;` | Set RX2 AF gain | Should control RX2 audio volume (-60..0 dB) | TCP CAT `ZZLE` |
| `RX_NR_ENABLE:0,true;` | Enable Noise Reduction on RX1 | Should toggle NR on/off | TCP CAT `ZZNE` |
| `RX_ANF_ENABLE:0,true;` | Enable Auto Notch Filter on RX1 | Should toggle ANF on/off | TCP CAT `ZZNT` |
| `RX_CHANNEL_SENSORS` | S-meter (RX signal strength in dBm) | Should push RX signal level periodically | Computed from IQ stream FFT data (passband power integration) |
| `TX_SENSORS` | TX power, SWR, mic level | Should push TX power/SWR periodically | Not available — no TX power readback in TCI mode |

We send these commands but they don't appear to have any effect.

## Missing from TCI protocol (no equivalent)

| Feature | Description | Current workaround |
|---|---|---|
| CTUN on/off | Click Tune enable/disable and state readback | TCP CAT `ZZCT` (read only — we detect state but can't toggle via TCI) |
| Thetis shutdown | Graceful application shutdown | TCP CAT `ZZBY` |
| TX profile select | Switch between TX profiles (by index) | TCP CAT `ZZTP` |
| Power on/off | Thetis radio power toggle (not START/STOP) | TCP CAT `ZZPS` (START/STOP starts/stops TCI streams, not Thetis power button) |

## IQ sample rate >384 kHz

| Request | Reason |
|---|---|
| `IQ_SAMPLERATE:1536000;` | The ANAN 7000DLE DDC provides 1536 kHz bandwidth. Currently TCI IQ streams are limited to 384 kHz. Supporting the full DDC bandwidth would give the remote client the same spectrum/waterfall coverage as the local Thetis display. |

## What works well

Everything else via TCI works reliably:
- **VFO/DDS**: frequency control and DDC center readback
- **Modulation**: mode switching (LSB/USB/CW/AM/FM/DIGU/DIGL/SAM)
- **Drive**: TX power control
- **Filters**: RX filter band control (`RX_FILTER_BAND`)
- **PTT**: transmit control (`TRX`) with `,tci` audio source
- **Audio streams**: RX1/RX2 audio (48 kHz float32 mono), TX audio via TX_CHRONO
- **IQ streams**: RX1/RX2 IQ data (384 kHz float32)
- **START/STOP**: TCI stream start/stop
- **RX_CHANNEL_ENABLE**: RX2 enable/disable detection
- **TUNE**: antenna tuner activation
- **AGC_MODE/AGC_GAIN**: AGC control per receiver
- **RIT/XIT**: RIT/XIT enable and offset
- **SQL_ENABLE/SQL_LEVEL**: squelch per receiver
- **VFO_LOCK**: VFO lock per receiver/channel
- **RX_BIN_ENABLE**: binaural mode (but useless over mono audio)
- **RX_APF_ENABLE**: audio peak filter per receiver
- **RX_NF_ENABLE**: manual notch filter (RX1 only confirmed working)
- **RX_NB_ENABLE**: noise blanker per receiver
- **TX_PROFILES_EX/TX_PROFILE_EX**: TX profile names and selection
- **MUTE/RX_MUTE**: global mute and per-receiver mute
- **MON_VOLUME**: monitor volume
- **TUNE_DRIVE**: tune power level
- **CW_KEYER_SPEED**: CW speed in WPM
- **RX_BALANCE**: audio balance per receiver
- **RX_SENSORS_ENABLE/TX_SENSORS_ENABLE**: push-based sensor notifications

## Thetis-specific TCI extensions (not in standard TCI spec)

### calibration_ex (Thetis v2.10.3+)

Sent by Thetis on connect and when calibration changes. Provides offsets needed to convert raw IQ FFT bins to calibrated dBm.

**Format:**
```
calibration_ex:{rx},{meter_cal},{display_cal},{xvtr_gain},{6m_gain},{tx_display};
```

| Field | Type | Description |
|-------|------|-------------|
| rx | u32 | Receiver: 0=RX1, 1=RX2 |
| meter_cal | f32 (6 decimals) | `_rx1_meter_cal_offset` — ADC/time-domain calibration. ANAN-7000DLE default: ~4.84 dB |
| display_cal | f32 (6 decimals) | `_rx1_display_cal_offset` — ADC/FFT calibration. Note: Thetis itself uses meter_cal for the panadapter, not this field |
| xvtr_gain | f32 (6 decimals) | Transverter RX gain offset (0.0 when no transverter active) |
| 6m_gain | f32 (6 decimals) | 6m LNA gain offset (raw setup value, typically 13 dB; applied negated only on 6m with LNA) |
| tx_display | f32 (6 decimals) | TX display calibration offset |

**Thetis total spectrum offset formula:**
```
total_offset = meter_cal + xvtr_gain + 6m_gain_applied + step_att_dB

Where:
  6m_gain_applied = -6m_gain (on 6m with LNA) or 0 (otherwise)
  step_att_dB = hardware ADC attenuator value (0-31 dB)
```

**Important:** The step attenuator value is NOT included in calibration_ex and is NOT readable via TCI or CAT (ZZRX returns 0). The IQ data sent via TCI is uncorrected for the attenuator. ThetisLink uses auto-calibration: comparing the calibrated TCI S-meter (avgdBm from `rx_channel_sensors_ex`) with the raw spectrum passband power to derive the dynamic ATT offset.

### rx_channel_sensors_ex (Thetis v2.10.3+)

Extended S-meter notification with three measurement types.

**Format:**
```
rx_channel_sensors_ex:{rx},{chan},{dBm},{avgdBm},{peakBinDbm};
```

| Field | Type | Description |
|-------|------|-------------|
| rx | u32 | Receiver: 0=RX1, 1=RX2 |
| chan | u32 | Channel (always 0) |
| dBm | f32 | SIGNAL_STRENGTH — WDSP peak detector (time-domain) + RXOffset |
| avgdBm | f32 | AVG_SIGNAL_STRENGTH — WDSP RMS average (time-domain) + RXOffset |
| peakBinDbm | f32 | SIGNAL_MAX_BIN — peak FFT bin in filter passband + RXOffset |

All three values are fully calibrated: `WDSP_raw + meter_cal + step_att + xvtr + 6m`.

**ThetisLink uses avgdBm** (field 3) for the S-meter — matches Thetis avg meter. peakBinDbm (field 4) is only the single highest FFT bin, too low for broadband noise.

### tx_profiles_ex / tx_profile_ex

TX profile management with human-readable names.

```
tx_profiles_ex:{name1},{name2},...;    # List of all profile names (brace-delimited)
tx_profile_ex:{name};                  # Current active profile name
```

## ATT/Preamp — not available via TCI

The step attenuator (0-31 dB) and preamp mode are controlled via HPSDR Protocol 2 only. There is no TCI command to read or write the attenuator value. CAT command `ZZRX` exists but returns 0 in Thetis (not connected to the step attenuator UI).

**Confirmed by Thetis developer (ramdor):** ATT is not readable/writable via TCI at this time. The IQ data is uncorrected.
