# TCI Protocol v2.0

**Transceiver Control Interface**

Series: Expert Electronics products external integration capabilities and APIs

12 January 2024

> Source: [ExpertSDR3/TCI on GitHub](https://github.com/ExpertSDR3/TCI)
> License: MIT

---

## 1 Introduction

### 1.1 Document Scope

TCI (Transceiver Control Interface) is a network interface for control, data transfer and synchronization between transceiver/receiver, contest loggers, digital mode software, skimmers and other software, as well as external power amplifiers, bandpass filter units, antenna switches, radio controllers and other devices.

### 1.2 Document Purpose

This document describes the TCI protocol, what it's for and how to use it.

### 1.3 Document Audience

The target audience is programmers who implement TCI protocol in their programs and devices.

### 1.4 Context

TCI was created as a modern alternative to the outdated COM port and audio cable interfaces. It uses a **full duplex WebSocket protocol** that runs on top of a TCP connection and serves for server-client communications, providing cross-platform connectivity.

- **Transceiver** works as a **server**
- All other software and devices as **clients**
- Server and clients can be on the same computer or separate devices on the local network

The TCI interface contains:
- Basic transceiver control commands (similar to CAT system)
- CW macros broadcast
- **Transceiver IQ stream output** to clients
- Spots from skimmers and Internet clusters
- **Audio signal input/output** for digital modes and voice

---

## 2 Updates and Changes in v2.0

### 2.1 Updated commands
- `TRX`

### 2.2 New Commands
- `VFO_LOCK`
- `RX_CHANNEL_SENSORS`

### Version History

| TCI ver | Changes | Date | ExpertSDR3 |
|---------|---------|------|------------|
| 1.9 | New: AUDIO_STREAM_SAMPLE_TYPE, AUDIO_STREAM_CHANNELS, AUDIO_STREAM_SAMPLES, DIGL_OFFSET, DIGU_OFFSET, TX_STREAM_AUDIO_BUFFERING. Updated audio streams section 3.4 | Jul 2022 | 0.13.0 |
| 1.9.1 | New: KEYER | May 2023 | 1.0.4 |
| 2.0 | Changes: VFO_LOCK, RX_CHANNEL_SENSORS. Updated TRX command | Jan 2024 | 1.0.7 |

---

## 3 General Overview

### 3.1 Interface Description

Any command is an **ASCII string** with a command name and argument list.

**Reserved characters:** `:`, `,`, `;`

**Command structure:**
1. Command name
2. `:` — separator between name and arguments
3. `,` — separator between arguments
4. `;` — end of command

If a command has no arguments, `;` follows the command name directly. Invalid commands are ignored. **Case does not matter.**

The ExpertSDR3 program acts as a server with multiple simultaneous client connections, synchronized by the server.

**Key behaviors:**
- On connect, client receives current status (initialization commands, then parameters)
- Server notifies all clients on parameter change (no polling needed)
- If a client sends a new state, server sets it and broadcasts to all clients
- Server acts as synchronizer — all clients automatically synchronized
- Minimizes network load by reducing traffic

**IQ streaming:** TCI implements transmission of receiver IQ stream to clients (for skimmer software, signal recording).

**Audio streaming:** TCI transmits receiver audio to clients and receives audio from clients for radio transmission (digital modes, voice macros, contest logging).

### 3.2 Working in CW mode

Two types of CW generation:
1. **CW Macros** — simple text transmission
2. **CW Message** — structured (prefix, callsign, suffix)

#### 3.2.1 CW Macro

Command: `cw_macros:arg1,arg2;`
- arg1 — receiver number
- arg2 — text message

Example: `cw_macros:0,TU RA6LH 599;`

**Abbreviations:** Use vertical brackets: `TEXT |SK| TEXT`

**Speed changes within text:** `<` decreases, `>` increases by 5 WPM steps:
```
ANY TEXT > TEXT +5WPM >>TEXT +15WPM
```

**Reserved character replacements:**
- `:` → `^`
- `,` → `~`
- `;` → `*`

**Terminal mode:**
- `cw_terminal:true;` — enable (stays in TX after macro completes)
- `cw_terminal:false;` — disable
- `cw_macros_empty;` — sent to client when last letter starts transmitting

#### 3.2.2 CW Message

Command: `cw_msg:arg1,arg2,arg3,arg4;`
- arg1 — receiver number
- arg2 — prefix
- arg3 — callsign
- arg4 — suffix

Example: `cw_msg:0,TU,RA6LH,599 004;`

Repeat callsign: `cw_msg:0,TU,RA6LH$2,599 004;` (callsign repeated twice)

**Callsign correction** (before fully transmitted): `cw_msg:RA6LH;`

After transmission complete, server sends: `callsign_send:RA6LH;`

**Stop transmission:** `cw_macros_stop;`

**Priority:** CW_MSG has higher priority than CW_MACROS. Sending CW_MSG during CW_MACROS immediately stops macros and starts message.

### 3.3 Types of TCI Commands

| Type | Description |
|------|-------------|
| **Initialization** | Sent to client upon connection (frequency range, modes, etc.) |
| **Bidirectional control** | Control basic parameters; server synchronizes all clients |
| **Unidirectional control** | Parameters unique to each client (audio streaming, spots, etc.) |
| **Notification** | Periodic sensor readings and signal meters |

### 3.4 Working with Audio Streams via TCI

Commands are transmitted as **strings**, audio streams as **byte streams** (WebSocket binary frames).

**Stream block structure (C):**
```c
struct Stream {
    uint32_t receiver;      // receiver number
    uint32_t sample_rate;   // sampling rate
    uint32_t format;        // sample type (StreamType enum)
    uint32_t codec;         // compression (not implemented), always 0
    uint32_t crc;           // checksum (not implemented), always 0
    uint32_t length;        // number of samples
    uint32_t type;          // stream type
    uint32_t channels;      // number of channels
    uint32_t reserv[8];     // reserved
    uint8_t  data[16384];   // samples
};
```

**Stream types:**
```c
enum StreamType {
    IQ_STREAM       = 0,  // Receiver IQ signal stream
    RX_AUDIO_STREAM = 1,  // Receiver audio stream
    TX_AUDIO_STREAM = 2,  // Audio stream for transmitter
    TX_CHRONO       = 3,  // Time marker for audio signal transmission
    LINEOUT_STREAM  = 4   // Linear audio output stream
};
```

**Sample types:**
```c
enum SampleType {
    INT16   = 0,  // 16-bit integer
    INT24   = 1,  // 24-bit integer
    INT32   = 2,  // 32-bit integer
    FLOAT32 = 3   // 32-bit float
};
```

**IQ stream:** Enable with `IQ_START`, number of complex samples = `Stream.length / Stream.channels`.

**RX audio stream:** Duplicates IQ stream but with configurable channels, sample format, and samples per packet.

**DIGL/DIGU modes:** 2 channels = complex signal, 1 channel = real signal.

**TX audio stream:** ExpertSDR3 sends `TX_CHRONO` timestamps. Client responds with `TX_AUDIO_STREAM` with the requested number of samples. If not ready, send zero samples (preferred over not sending).

### 3.5 Particularities of TCI Server Operation in ExpertSDR3

- Server syncs all connected clients
- **Priority:** ExpertSDR3 has highest priority, then first client to change a parameter
- **200ms monopolization:** When ExpertSDR3 or a client changes a parameter, it's locked for 200ms
- **Band change:** Setting a frequency in a different band triggers ExpertSDR3 to reinstate saved settings for that band and broadcast to all clients
- **CW_MSG priority** over CW_MACROS

---

## 4 TCI Commands List

### 4.1 Initialization Commands

| Command | Description | Format | Example |
|---------|-------------|--------|---------|
| `VFO_LIMITS` | Device operating frequency range | `VFO_LIMITS:low_hz,high_hz;` | `VFO_LIMITS:10000,30000000;` |
| `IF_LIMITS` | IF filter frequency limits (VFOA only) | `IF_LIMITS:low_hz,high_hz;` | `IF_LIMITS:-48000,48000;` |
| `TRX_COUNT` | Number of receivers/transceivers | `TRX_COUNT:count;` | `TRX_COUNT:2;` |
| `CHANNEL_COUNT` | Number of receive channels per receiver (A/B/C) | `CHANNEL_COUNT:count;` | `CHANNEL_COUNT:2;` |
| `DEVICE` | Device name | `DEVICE:name;` | `DEVICE:SunSDR2DX;` |
| `RECEIVE_ONLY` | Receiver only (true) or transceiver (false) | `RECEIVE_ONLY:bool;` | `RECEIVE_ONLY:true;` |
| `MODULATIONS_LIST` | Supported modulations | `MODULATIONS_LIST:m1,m2,...;` | `MODULATIONS_LIST:AM,LSB,USB,FM;` |
| `PROTOCOL` | Protocol version | `PROTOCOL:program,version;` | `PROTOCOL:ExpertSDR3,1.9;` |
| `READY` | Initialization complete | `READY;` | `READY;` |

**Note:** `IF_LIMITS` is sent when connecting or when sample rate changes.

### 4.2 Bidirectional Control Commands

#### START / STOP
| Command | Description | Example |
|---------|-------------|---------|
| `START` | Device start | `START;` |
| `STOP` | Device stop | `STOP;` |

#### DDS — Receiver Center Frequency
- Set: `DDS:receiver,freq_hz;`
- Read: `DDS:receiver;`
- Reply: `DDS:receiver,freq_hz;`
- Example: `DDS:0,7100000;`

#### IF — IF Tuning Filter Frequency
- Set: `IF:receiver,channel,freq_hz;`
- Read: `IF:receiver,channel;`
- Reply: `IF:receiver,channel,freq_hz;`
- Example: `IF:0,1,12500;` `IF:0,1,-17550;`

#### VFO — Receiver Frequency
- Set: `VFO:receiver,channel,freq_hz;`
- Read: `VFO:receiver,channel;`
- Reply: `VFO:receiver,channel,freq_hz;`
- Example: `VFO:0,1,7100000;` `VFO:1,0,14250000;`

#### MODULATION — Mode Switching
- Set: `MODULATION:receiver,mode_str;`
- Read: `MODULATION:receiver;`
- Reply: `MODULATION:receiver,mode_str;`
- Example: `MODULATION:0,LSB;` `MODULATION:1,NFM;`

#### TRX — RX/TX Switching
- Set: `TRX:receiver,bool[,source];`
- Read: `TRX:receiver;`
- Reply: `TRX:receiver,bool;`

Signal sources (optional arg3):
| Source | Description |
|--------|-------------|
| `tci` | Take signal from TCI audio stream |
| `mic1` | Mic1 input |
| `mic2` | Mic2 input |
| `micPC` | MicPC input |
| `ecoder2` | E-Coder2 input |

Examples: `TRX:0,true;` `TRX:0,true,tci;` `TRX:0,false;` `TRX:0,true,micpc;`

**Note:** If TCI source is specified, TCI audio stream must be enabled. Without arg3, microphone selected in ExpertSDR3 is used.

#### TUNE — Receive/Tune Mode
- Set: `TUNE:receiver,bool;`
- Example: `TUNE:0,true;` `TUNE:0,false;`

#### DRIVE — Output Power (0-100)
- Set: `DRIVE:receiver,value;`
- Read: `DRIVE:receiver;`
- Example: `DRIVE:0,30;` `DRIVE:0,75;`

#### TUNE_DRIVE — Tune Mode Output Power (0-100)
- Set: `TUNE_DRIVE:receiver,value;`
- Example: `TUNE_DRIVE:0,30;`

#### RIT_ENABLE / XIT_ENABLE
- Set: `RIT_ENABLE:receiver,bool;` / `XIT_ENABLE:receiver,bool;`
- Example: `RIT_ENABLE:0,true;`

#### SPLIT_ENABLE
- Set: `SPLIT_ENABLE:receiver,bool;`
- Example: `SPLIT_ENABLE:0,true;`

#### RIT_OFFSET / XIT_OFFSET
- Set: `RIT_OFFSET:receiver,freq_hz;` / `XIT_OFFSET:receiver,freq_hz;`
- Example: `RIT_OFFSET:0,500;` `XIT_OFFSET:0,-350;`

#### RX_CHANNEL_ENABLE — Enable VFO B
- Set: `RX_CHANNEL_ENABLE:receiver,channel,bool;`
- Read: `RX_CHANNEL_ENABLE:receiver,channel;`
- Example: `RX_CHANNEL_ENABLE:0,1,true;`
- **Note:** Channel A is always on; only channel B can be controlled.

#### RX_FILTER_BAND — RX Filter Width
- Set: `RX_FILTER_BAND:receiver,low_hz,high_hz;`
- Read: `RX_FILTER_BAND:receiver;`
- Example: `RX_FILTER_BAND:0,30,2700;` `RX_FILTER_BAND:1,-2900,-70;`

#### CW_MACROS_SPEED — CW Speed (WPM)
- Set: `CW_MACROS_SPEED:wpm;`
- Read: `CW_MACROS_SPEED;`
- Example: `CW_MACROS_SPEED:42;`

#### CW_MACROS_DELAY — CW TX Delay (ms)
- Set: `CW_MACROS_DELAY:ms;`
- Example: `CW_MACROS_DELAY:100;`
- Adjusts delay between TX initiation and actual transmission.

#### CW_KEYER_SPEED — CW Keyer Speed (WPM)
- Set: `CW_KEYER_SPEED:wpm;`
- Example: `CW_KEYER_SPEED:35;`
- **Note:** Sent only by TCI client.

#### VOLUME — Main Volume (-60 to 0 dB)
- Set: `VOLUME:db;`
- Read: `VOLUME;`
- Example: `VOLUME:-12;`
- At -60 dB there is no sound.

#### MUTE — Main Volume Mute
- Set: `MUTE:bool;`
- Read: `MUTE;`
- Example: `MUTE:true;`

#### RX_MUTE — Mute Specific Receiver
- Set: `RX_MUTE:receiver,bool;`
- Example: `RX_MUTE:0,true;`

#### RX_VOLUME — Per-Channel Volume (-60 to 0 dB)
- Set: `RX_VOLUME:receiver,channel,db;`
- Read: `RX_VOLUME:receiver,channel;`
- Example: `RX_VOLUME:0,1,-6;`

#### RX_BALANCE — Per-Channel Balance (-40 to 40 dB)
- Set: `RX_BALANCE:receiver,channel,db;`
- Example: `RX_BALANCE:0,1,-6;`
- Negative = decrease left, positive = decrease right.

#### MON_VOLUME — TX Monitor Volume (-60 to 0 dB)
- Set: `MON_VOLUME:db;`
- Example: `MON_VOLUME:-12;`

#### MON_ENABLE — TX Monitor Enable
- Set: `MON_ENABLE:bool;`
- Example: `MON_ENABLE:true;`

#### AGC_MODE — Receiver AGC Mode
- Set: `AGC_MODE:receiver,mode;`
- Modes: `normal`, `fast`, `off`
- Example: `AGC_MODE:0,normal;`

#### AGC_GAIN — Receiver AGC Gain (-20 to 120 dB)
- Set: `AGC_GAIN:receiver,db;`
- Example: `AGC_GAIN:0,87;`

#### RX_NB_ENABLE — Noise Blanker
- Set: `RX_NB_ENABLE:receiver,bool;`
- Example: `RX_NB_ENABLE:0,true;`

#### RX_NB_PARAM — NB Parameters
- Set: `RX_NB_PARAM:receiver,threshold(1-100),duration(1-300);`
- Example: `RX_NB_PARAM:0,70,25;`

#### RX_BIN_ENABLE — Binaural (Pseudo Stereo)
- Set: `RX_BIN_ENABLE:receiver,bool;`
- Example: `RX_BIN_ENABLE:0,true;`

#### RX_NR_ENABLE — Noise Reduction
- Set: `RX_NR_ENABLE:receiver,bool;`
- Example: `RX_NR_ENABLE:0,true;`

#### RX_ANC_ENABLE — Adaptive Noise Cancellation
- Set: `RX_ANC_ENABLE:receiver,bool;`
- Example: `RX_ANC_ENABLE:0,true;`

#### RX_ANF_ENABLE — Automatic Notch Filter
- Set: `RX_ANF_ENABLE:receiver,bool;`
- Example: `RX_ANF_ENABLE:0,true;`

#### RX_APF_ENABLE — Analog Peak Filter
- Set: `RX_APF_ENABLE:receiver,bool;`
- Example: `RX_APF_ENABLE:0,true;`

#### RX_DSE_ENABLE — Digital Surround Effect (CW)
- Set: `RX_DSE_ENABLE:receiver,bool;`
- Example: `RX_DSE_ENABLE:0,true;`

#### RX_NF_ENABLE — Notch Filters Module
- Set: `RX_NF_ENABLE:receiver,bool;`
- Example: `RX_NF_ENABLE:0,true;`

#### LOCK — Tuning Frequency Lock
- Set: `LOCK:receiver,bool;`
- Example: `LOCK:0,true;`

#### SQL_ENABLE — Squelch Enable
- Set: `SQL_ENABLE:receiver,bool;`
- Example: `SQL_ENABLE:0,true;`

#### SQL_LEVEL — Squelch Threshold (-140 to 0 dB)
- Set: `SQL_LEVEL:receiver,db;`
- Example: `SQL_LEVEL:0,-83;`

#### DIGL_OFFSET / DIGU_OFFSET — Digital Mode Frequency Offset (0-4000 Hz)
- Set: `DIGL_OFFSET:hz;` / `DIGU_OFFSET:hz;`
- Read: `DIGL_OFFSET;` / `DIGU_OFFSET;`
- Example: `DIGL_OFFSET:1500;` `DIGU_OFFSET:2200;`

### 4.3 Unidirectional Control Commands

#### TX_ENABLE — TX Permission (server → client)
- Reply: `TX_ENABLE:receiver,bool;`
- Example: `TX_ENABLE:0,true;`
- Sent on connect and when band changes.

#### CW_MACROS_SPEED_UP / CW_MACROS_SPEED_DOWN (client → server)
- Set: `CW_MACROS_SPEED_UP:wpm;` / `CW_MACROS_SPEED_DOWN:wpm;`
- Example: `CW_MACROS_SPEED_UP:7;`

#### SPOT — Display Spot on Panorama (client → server)
- Set: `SPOT:callsign,mode,freq_hz,argb_color,text;`
- Example: `SPOT:RN6LHF,CW,7100000,16711680,ANY_TEXT;`

#### SPOT_DELETE — Delete Spot (client → server)
- Set: `SPOT_DELETE:callsign;`
- Example: `SPOT_DELETE:RN6LHF;`

#### SPOT_CLEAR — Delete All Spots (client → server)
- Set: `SPOT_CLEAR;`

#### IQ_SAMPLERATE — IQ Stream Sample Rate (client → server)
- Set: `IQ_SAMPLERATE:rate_hz;`
- Supported: 48000, 96000, 192000, 384000 Hz
- Example: `IQ_SAMPLERATE:48000;`

#### AUDIO_SAMPLERATE — Audio Stream Sample Rate (client → server)
- Set: `AUDIO_SAMPLERATE:rate_hz;`
- Supported: 8000, 12000, 24000, 48000 Hz
- Example: `AUDIO_SAMPLERATE:12000;`

#### IQ_START / IQ_STOP — IQ Stream Control (client → server)
- `IQ_START:receiver;` / `IQ_STOP:receiver;`
- Example: `IQ_START:0;`

#### AUDIO_START / AUDIO_STOP — Audio Stream Control (client → server)
- `AUDIO_START:receiver;` / `AUDIO_STOP:receiver;`
- Example: `AUDIO_START:0;`

#### LINE_OUT_START / LINE_OUT_STOP — Line Out Stream (client → server)
- `LINE_OUT_START:receiver;` / `LINE_OUT_STOP:receiver;`

#### LINE_OUT_RECORDER_START — Start Line Out Recording (client → server)
- `LINE_OUT_RECORDER_START:receiver,max_seconds;`
- Max 300 seconds. Recording is deleted when time expires unless saved.

#### LINE_OUT_RECORDER_SAVE — Save Recording (client → server)
- `LINE_OUT_RECORDER_SAVE:receiver,filepath;`
- Formats: WAVE, MP3
- Windows: replace `:` with `|` in paths

#### LINE_OUT_RECORDER_BREAK — Stop and Delete Recording (client → server)
- `LINE_OUT_RECORDER_BREAK:receiver;`

#### AUDIO_STREAM_SAMPLE_TYPE — Audio Stream Sample Format (client → server)
- `AUDIO_STREAM_SAMPLE_TYPE:format;`
- Formats: `int16`, `int24`, `int32`, `float32`
- Default: `float32`

#### AUDIO_STREAM_CHANNELS — Audio Stream Channels (client → server)
- `AUDIO_STREAM_CHANNELS:count;`
- Supported: 1 or 2
- Default: 2

#### AUDIO_STREAM_SAMPLES — Samples per Packet (client → server)
- `AUDIO_STREAM_SAMPLES:count;`
- Range: 100-2048

Default samples per rate:
| Sample Rate | Default Samples | Minimum Recommended |
|-------------|----------------|---------------------|
| 48 kHz | 2048 | 512 |
| 24 kHz | 1024 | 256 |
| 12 kHz | 512 | 128 |
| 8 kHz | 256 | 100 |

Playback duration should not be less than 10ms.

#### TX_STREAM_AUDIO_BUFFERING — TX Buffering Timeout (client → server)
- `TX_STREAM_AUDIO_BUFFERING:ms;`
- Range: 50-500ms
- Default: 50ms

### 4.4 Notification Commands

#### CLICKED_ON_SPOT — Spot Click (server → client, legacy)
- `CLICKED_ON_SPOT:callsign,freq_hz;`

#### RX_CLICKED_ON_SPOT — Spot Click (server → client)
- `RX_CLICKED_ON_SPOT:receiver,channel,callsign,freq_hz;`

#### TX_FOOTSWITCH — PTT Footswitch State (server → client)
- `TX_FOOTSWITCH:receiver,bool;`
- Example: `TX_FOOTSWITCH:0,true;`

#### TX_FREQUENCY — Current TX Frequency (server → client)
- `TX_FREQUENCY:freq_hz;`
- Example: `TX_FREQUENCY:7140000;`

#### APP_FOCUS — ExpertSDR3 Window Focus (server → client)
- `APP_FOCUS:bool;`

#### SET_IN_FOCUS — Bring ExpertSDR3 to Focus (client → server)
- `SET_IN_FOCUS;`

#### KEYER — CW Key State (client → server)
- `KEYER:receiver,bool,prev_char_length_ms;`
- Example: `KEYER:0,true,0;` (first press) `KEYER:0,false,142;` (release)
- Designed for CW via COM-port with RadioSync. Third argument preserves dot/dash length timing.

#### RX_SENSORS_ENABLE — Enable RX Signal Level Reports (client → server)
- `RX_SENSORS_ENABLE:bool[,interval_ms];`
- Interval: 30-1000ms
- Example: `RX_SENSORS_ENABLE:true,200;`

#### TX_SENSORS_ENABLE — Enable TX Signal Reports (client → server)
- `TX_SENSORS_ENABLE:bool[,interval_ms];`
- Example: `TX_SENSORS_ENABLE:true,200;`

#### RX_SENSORS — RX Signal Level (server → client, **deprecated**)
- `RX_SENSORS:receiver,level_dbm;`
- Example: `RX_SENSORS:0,-71.5;`
- **Replaced by RX_CHANNEL_SENSORS.**

#### TX_SENSORS — TX Signal Parameters (server → client)
- `TX_SENSORS:receiver,mic_dbm,power_w_rms,power_w_peak,swr;`
- Example: `TX_SENSORS:0,-27.2,47.4,67.5,1.7;`

### 4.5 New Commands in v2.0

#### VFO_LOCK — Frequency Tuning Lock Notification (server → client)
- `VFO_LOCK:receiver,channel,bool;`
- Example: `VFO_LOCK:0,1,true;`

#### RX_CHANNEL_SENSORS — Per-Channel Signal Level (server → client)
- `RX_CHANNEL_SENSORS:receiver,channel,level_dbm;`
- Example: `RX_CHANNEL_SENSORS:0,0,-71.5;` `RX_CHANNEL_SENSORS:1,1,-112.7;`
