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
