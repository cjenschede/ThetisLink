# ThetisLink Desktop Client UI

Detailed documentation of `sdr-remote-client/src/ui.rs` (~5,668 LOC).

## Window Architecture

```mermaid
graph TB
    subgraph "Always Active"
        MAIN[Main Window<br/>ThetisLink Client]
    end

    subgraph "Conditional (Popout)"
        RX1_POP[RX1 / VFO-A Popout<br/>Spectrum + Controls]
        RX2_POP[RX2 / VFO-B Popout<br/>Spectrum + Controls]
        JOINED[Joined Popout<br/>RX1 + RX2 together]
    end

    MAIN -->|spectrum_popout=true| RX1_POP
    MAIN -->|rx2_popout=true| RX2_POP
    RX1_POP -->|popout_joined=true| JOINED
    RX2_POP -->|popout_joined=true| JOINED
```

### Popout State Machine

```mermaid
stateDiagram-v2
    [*] --> NoPopout: Start

    NoPopout --> RX1Solo: Popout button click<br/>(spectrum_popout=true)
    RX1Solo --> NoPopout: Popout button click<br/>(spectrum_popout=false)

    RX1Solo --> BothSplit: RX2 enable + auto-popout<br/>(rx2_popout=true)
    BothSplit --> RX1Solo: Close RX2 popout

    BothSplit --> BothJoined: Join button<br/>(popout_joined=true)
    BothJoined --> BothSplit: Split button<br/>(popout_joined=false)

    BothJoined --> NoPopout: Close popout
    BothSplit --> NoPopout: Close both

    state NoPopout {
        [*] --> MainAll
        MainAll: Main window shows everything
        MainAll: VFO A + inline spectrum
        MainAll: Optional RX2 inline
    }

    state RX1Solo {
        [*] --> MainMin
        MainMin: Main window: basic controls
        MainMin: Popout: VFO A + spectrum
    }

    state BothSplit {
        [*] --> TwoWindows
        TwoWindows: Popout 1: VFO A + spectrum
        TwoWindows: Popout 2: VFO B + spectrum
        TwoWindows: Join button visible
    }

    state BothJoined {
        [*] --> OneWindow
        OneWindow: Left: VFO A controls
        OneWindow: Right: VFO B controls
        OneWindow: Top: RX1 spectrum
        OneWindow: Bottom: RX2 spectrum
    }
```

## Main Window Layout

```mermaid
graph TD
    subgraph "Main Window"
        TOP[Top Panel]
        RADIO[Radio Screen]
        DEVICES[Devices Screen]
        LOG[Log Panel]

        TOP --> RADIO
        TOP --> DEVICES
        RADIO --> LOG
        DEVICES --> LOG
    end

    subgraph "Top Panel"
        PTT[PTT Button + Spacebar]
        TUNE[Tune Button<br/>if tuner available]
        PA_STATUS[PA Status<br/>SPE/RF2K compact]
        RX_VOL[RX Volume Slider<br/>Thetis ZZLA]
        TX_GAIN[TX Gain Slider]
    end

    subgraph "Radio Screen"
        CONN[Connection: address + button]
        STATUS[Status: RTT, jitter, loss]
        VFO_A[VFO A: frequency + S-meter]
        BANDS[Band buttons: 160m–6m]
        MODE[Mode: LSB/USB/CW/AM/FM/DIG]
        FILTER[Filter: low/high Hz + presets]
        NR_ANF[NR level + ANF toggle]
        DRIVE[Drive level + Power button]
        VOLUME[Volume slider<br/>role depends on popout state]
        RX2_SECT[RX2 section<br/>if rx2_enabled]
        SPEC[Spectrum + Waterfall<br/>if not popout]
    end

    subgraph "Devices Screen"
        TABS[Tabs: Amplitec|Tuner|SPE|RF2K|UB|Rotor]
        DEV_CONTENT[Device-specific UI]
    end
```

## Render Functions Overview

### Shared Functions (used by multiple windows)

| Function | Lines | Used By | Description |
|----------|-------|---------|-------------|
| `render_rx1_controls()` | 2421–2664 | Popout solo, Joined left | VFO A: freq scroll, mode, S-meter, band, filter, NR/ANF, volume |
| `render_rx2_controls_with_split()` | 2674–2918 | Main (inline), Popout solo, Joined right | VFO B: freq, mode, S-meter, band, filter, NR/ANF, volume |
| `render_spectrum_content()` | 2289–2420 | Main (inline), Popout | RX1 spectrum + waterfall + sliders |
| `render_rx2_spectrum_only()` | 2926–3069 | RX2 popout, Joined bottom | RX2 spectrum + waterfall |
| `smeter_bar()` | 4638–4795 | Everywhere S-meter is needed | S-meter visualization with dB scale |
| `render_freq_scroll()` | ~4500 | Popout VFOs only | Per-digit frequency scroll |

### Wrapper Functions

| Function | Lines | Window | Content |
|----------|-------|--------|---------|
| `render_rx1_popout_content()` | 2665–2670 | RX1 popout (solo) | rx1_controls + spectrum_content |
| `render_rx2_content()` | 2919–2923 | RX2 popout (solo) | rx2_controls + rx2_spectrum |

### Device Functions

| Function | Lines | LOC | Device |
|----------|-------|-----|--------|
| `render_devices_screen()` | 1092–1124 | ~30 | Tab selector |
| `render_device_amplitec()` | 1125–1223 | ~100 | Amplitec 6/2 antenna switch |
| `render_device_tuner()` | 1224–1290 | ~70 | JC-4s tuner |
| `render_device_spe()` | 1291–1488 | ~200 | SPE Expert 1.3K-FA |
| `render_device_rf2k()` | 1489–2012 | ~520 | RF2K-S PA (incl. debug/drive) |
| `render_device_ultrabeam()` | 2013–2153 | ~140 | UltraBeam RCU-06 |
| `render_device_rotor()` | 2154–2287 | ~130 | EA7HG Visual Rotor |

### Spectrum Functions

| Function | Lines | Description |
|----------|-------|-------------|
| `spectrum_plot()` | 4797–5185 | RX1 spectrum line + waterfall + freq scale |
| `rx2_spectrum_plot()` | 5189–5530 | RX2 spectrum (separate, duplicate logic) |
| `WaterfallRingBuffer` | ~5530–5668 | Ring buffer for waterfall rows + egui texture |

## Function per Window Matrix

| Function | Main | RX1 Solo | RX2 Solo | Joined |
|----------|:----:|:--------:|:--------:|:------:|
| render_rx1_controls | - | yes | - | left |
| render_rx2_controls_with_split | inline | - | yes | right |
| render_spectrum_content | inline | yes | - | top |
| render_rx2_spectrum_only | - | - | yes | bottom |
| smeter_bar | yes | yes | yes | yes |
| render_freq_scroll | - | yes | yes | yes |
| Band buttons | yes | yes | yes | yes |
| Mode selector | yes | yes | yes | yes |
| Filter controls | yes | yes | yes | yes |
| Volume slider | yes | yes | yes | yes |

**Note:** `render_freq_scroll` (per-digit scroll) is **only** in popout windows. The main window has a plain clickable frequency label.

## Volume Routing

```mermaid
graph TD
    subgraph "Both Popout"
        MA[Main Window:<br/>Master slider<br/>local_volume]
        VA[RX1 Popout:<br/>VFO A slider<br/>vfo_a_volume]
        VB[RX2 Popout:<br/>VFO B slider<br/>vfo_b_volume]

        MA --> MIX1[RX1: rx × vfoA × master]
        MA --> MIX2[RX2: rx2 × vfoB × master]
        VA --> MIX1
        VB --> MIX2
    end

    subgraph "RX1 Popout Only"
        MA2[Main Window:<br/>VFO A slider<br/>vfo_a_volume]
        FORCE1[Master = 100%<br/>VFO B = muted]

        MA2 --> MIX3[RX1: rx × vfoA × 1.0]
        FORCE1 --> MIX3
    end

    subgraph "No Popout"
        MA3[Main Window:<br/>VFO A slider<br/>vfo_a_volume]
        FORCE2[Master = 100%]

        MA3 --> MIX4[RX1: rx × vfoA × 1.0]
        FORCE2 --> MIX4
    end
```

### Routing Rules (in update())

```
if spectrum_popout AND rx2_popout:
    → Main window slider = Master (local_volume)
    → Popout sliders = VFO A / VFO B

if spectrum_popout AND NOT rx2_popout:
    → Master forced to 1.0
    → VFO B forced to 0.001 (muted)
    → Main window slider = VFO A

if NOT spectrum_popout:
    → Master forced to 1.0
    → Main window slider = VFO A
```

## Band Memory System

```mermaid
sequenceDiagram
    participant UI as User
    participant App as SdrRemoteApp
    participant Mem as band_mem HashMap
    participant Eng as Engine

    UI->>App: Click "20m" band button
    App->>App: save_current_band()<br/>save current freq/mode/filter/NR
    App->>Mem: band_mem["40m"] = {7073000, USB, -100..2800, NR2}

    App->>Mem: Lookup band_mem["20m"]
    Mem->>App: {14200000, USB, -100..2800, NR2}

    App->>Eng: SetMode(USB)
    App->>Eng: SetFrequency(14200000)
    App->>Eng: SetControl(FilterLow, -100)
    App->>Eng: SetControl(FilterHigh, 2800)
    App->>Eng: SetControl(NR, 2)

    App->>App: save_full_config()
```

### BandMemory Struct

```rust
struct BandMemory {
    frequency_hz: u64,    // Last used frequency
    mode: u8,             // LSB/USB/CW/AM/FM/DIG
    filter_low_hz: i32,   // Filter lower bound (Hz)
    filter_high_hz: i32,  // Filter upper bound (Hz)
    nr_level: u8,         // Noise Reduction level (0-4)
}
```

### Storage in Config

```
band_mem_40m=7073000:1:-100:2800:2
band_mem_20m=14200000:1:-100:2800:2
band_mem_80m=3573000:0:-2800:100:0
```

Format: `band_mem_{label}={freq}:{mode}:{filter_low}:{filter_high}:{nr}`

## SdrRemoteApp State Groups

### Total: ~280+ fields

| Group | Count | Description |
|-------|-------|-------------|
| Connection & UI | ~20 | server_input, connected, show_log, show_devices |
| Audio volumes | ~6 | rx_volume, vfo_a/b, local, tx_gain, rx2_volume |
| Radio state (cache) | ~18 | frequency, mode, smeter, power, filters, NR/ANF |
| Spectrum RX1 | ~22 | bins, center, span, zoom, pan, waterfall, auto_ref |
| Spectrum RX2 | ~20 | Copy of RX1 with rx2_ prefix |
| RX2 / VFO-B | ~15 | frequency, mode, smeter, filters, popout state |
| Amplitec | ~5 | connected, switch_a/b, labels, log |
| Tuner | ~4 | connected, state, can_tune, tune_freq |
| SPE Expert | ~16 | connected, state, power, SWR, temp, antenna, ... |
| RF2K-S basic | ~25 | connected, operate, band, freq, power, SWR, ... |
| RF2K-S debug | ~20 | bias, PSU, uptime, error, drive config tables |
| UltraBeam | ~12 | connected, freq, band, direction, motors, elements |
| Rotor | ~6 | connected, angle, rotating, target |
| Band memory | ~4 | band_mem, current_band, wf_contrast_per_band |
| Channels | 2 | state_rx (watch), cmd_tx (mpsc) |

## Duplicated Code (Refactoring Candidates)

### 1. Spectrum Rendering (~400 LOC × 2)
- `render_spectrum_content()` (RX1) and `render_rx2_spectrum_only()` (RX2)
- Identical logic with different variables (spectrum_* vs rx2_spectrum_*)
- **Refactoring:** Parameterize with a `SpectrumState` struct

### 2. Spectrum Plot (~400 LOC × 2)
- `spectrum_plot()` (RX1) and `rx2_spectrum_plot()` (RX2)
- Same plot logic, waterfall, freq scale
- **Refactoring:** One function with `SpectrumState` parameter

### 3. Controls Rendering (~250 LOC × 2)
- `render_rx1_controls()` and `render_rx2_controls_with_split()`
- Largely identical: freq, mode, S-meter, band, filter, NR/ANF
- Differences: VFO A vs B variables, Split button, is_popout parameter
- **Refactoring:** One function with `VfoContext` (rx1/rx2 enum + state refs)

### 4. Band Memory (~60 LOC × 2)
- `save_current_band()` / `restore_band()` (RX1)
- `save_current_band_rx2()` / `restore_band_rx2()` (RX2)
- **Refactoring:** Parameterize with VFO identifier

### 5. Device State (~100 fields)
- All device fields directly in SdrRemoteApp
- **Refactoring:** Per-device struct (AmplitecState, TunerState, etc.)

## Config Persistence

### Saved Fields (thetislink-client.conf)

| Field | Type | Default |
|-------|------|---------|
| server | String | "" |
| volume (rx) | f32 | 0.5 |
| tx_gain | f32 | 0.5 |
| vfo_a_volume | f32 | 1.0 |
| vfo_b_volume | f32 | 1.0 |
| local_volume | f32 | 1.0 |
| rx2_volume | f32 | 0.2 |
| input_device | String | "" |
| output_device | String | "" |
| agc_enabled | bool | false |
| spectrum_enabled | bool | false |
| spectrum_ref_db | f32 | -20.0 |
| spectrum_range_db | f32 | 100.0 |
| auto_ref_enabled | bool | false |
| waterfall_contrast | f32 | 1.2 |
| wf_contrast_per_band | HashMap | {} |
| rx2_spectrum_* | | (same set) |
| popout_joined | bool | false |
| band_mem_{label} | BandMemory | per band |
| tx_profiles | Vec | [] |
| memories | [Memory; 5] | empty |

### Save Triggers

- `save_full_config()` is called on:
  - Volume change (all sliders)
  - Band switch (save band memory)
  - Spectrum setting change
  - Popout join/split
  - Config-related UI interactions
