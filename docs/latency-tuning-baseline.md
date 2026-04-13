# Latency Tuning Baseline — v0.4.0 build 3

Oorspronkelijke waarden voor elke optimalisatie. Per nummer terug te zetten.

## 1. Sinc Resampler (client — sdr-remote-logic/src/engine.rs)

3 resamplers (RX1, RX2, TX input) met identieke params:

```rust
sinc_len: 128,
f_cutoff: 0.95,
oversampling_factor: 128,
interpolation: rubato::SincInterpolationType::Cubic,
window: rubato::WindowFunction::Blackman,
```

Group delay: ~80-100ms per richting (160-200ms roundtrip).

## 2. Client Jitter Buffer (client — sdr-remote-logic/src/engine.rs)

```rust
// RX1:
let mut jitter_buf = JitterBuffer::new(5, 40);   // line 119
// RX2:
let mut rx2_jitter_buf = JitterBuffer::new(5, 40); // line 116
```

Min 5 frames = 100ms, max 40 frames = 800ms.

## 3. Command Broadcast Tick (server — sdr-remote-server/src/network.rs)

```rust
let mut freq_tick = interval(Duration::from_millis(500));  // line 221
```

VFO/mode/state updates naar clients elke 500ms.

## 4. Spectrum EMA Smoothing (server — sdr-remote-server/src/spectrum.rs)

```rust
let decay = 0.6f32;  // ~120ms time constant at ~12 FFT/sec
```

## 5. Server Jitter Buffer (server — sdr-remote-server/src/network.rs)

```rust
let mut jitter_buf = JitterBuffer::new(3, 20);  // line 708
```

Min 3 frames = 60ms, max 20 frames = 400ms.

## 6. PTT Timing TCI (server — sdr-remote-server/src/ptt.rs)

```rust
const PTT_TAIL_MIN_MS_TCI: u64 = 40;
const PTT_TAIL_MARGIN_MS_TCI: u64 = 20;
const PTT_PREFILL_MS_TCI: u64 = 20;
```
