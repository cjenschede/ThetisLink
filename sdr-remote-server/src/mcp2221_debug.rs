// SPDX-License-Identifier: GPL-2.0-or-later

//! MCP2221A bridge for the StockCorner JC-4s/JC-3s tuner.
//!
//! GP2 drives the transistor that pulls the grey "start" wire low; GP1 reads
//! the yellow "tune-status" wire through a 1 M + 1 M 1:1 divider. The operator
//! sets two voltage thresholds (switch level + hysteresis) on the yellow line;
//! samples below `threshold - hyst/2` count as tune-active, samples above
//! `threshold + hyst/2` count as tune-idle. See `tuner.rs` for the full tune
//! sequence and `docs/internal/referentie/MCP2221A-JC4s-wiring.md` for the
//! schema.
//!
//! USB I/O via `mcp2221-hal` is synchronous and fast (sub-ms per call), so
//! the tuner thread and the UI thread both call in directly. State is guarded
//! by a single `std::sync::Mutex`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mcp2221_hal::gpio::{GpioChanges, GpioDirection, LogicLevel};
use mcp2221_hal::settings::{Gp1Mode, Gp2Mode};
use mcp2221_hal::MCP2221;

/// Minimum interval between ADC polls - keeps USB HID traffic bounded even
/// when the UI repaints at 30 FPS. 100 ms is plenty for the per-tuner row.
const ADC_POLL_MIN_MS: u64 = 100;

/// Voltage divider ratio undone in display (R1 = R2 = 1 MΩ -> ×2).
const DIVIDER_RATIO: f32 = 2.0;
/// MCP2221A ADC reference (Vdd-relative, default 3.3 V on Adafruit breakout).
const ADC_VREF: f32 = 3.3;
/// 10-bit ADC full-scale.
const ADC_FULL_SCALE: f32 = 1023.0;

/// Default tune-active switch threshold on the yellow wire, in volts.
pub const DEFAULT_THRESHOLD_V: f32 = 2.25;
/// Default hysteresis around the threshold, in volts. With the default 2.25 V
/// switch level this gives active < 2.00 V and idle > 2.50 V.
pub const DEFAULT_HYSTERESIS_V: f32 = 0.50;

#[derive(Debug, Clone)]
pub enum Status {
    /// Never tried to open the device yet.
    NotInitialized,
    /// Device open + pin modes configured.
    Connected,
    /// Open failed or a later call errored. Field carries the reason for
    /// display.
    Error(String),
}

#[derive(Debug, Clone)]
pub struct DebugSnapshot {
    pub status: Status,
    /// Last commanded GP2 level (Low=false, High=true). Mirrors what the
    /// user clicked; not a read-back from the chip.
    pub gp2_high: bool,
    /// Most recent ADC reading on GP1, raw 0-1023. None until a successful
    /// poll has happened.
    pub last_adc_raw: Option<u16>,
    /// Voltage on the yellow wire (`raw * Vref / 1023 * divider_ratio`).
    pub last_yellow_v: Option<f32>,
    /// Wall-clock age of the ADC reading, for "stale" badges in the UI.
    pub last_adc_age: Option<Duration>,
    /// Monotonic timestamp of the last ADC poll. Used by the tuner thread
    /// to dedup rate-limited cached samples: `snapshot()` returns the same
    /// cached value across multiple calls within `ADC_POLL_MIN_MS`, so the
    /// thread must compare timestamps before counting a consecutive edge.
    pub last_adc_at: Option<Instant>,
    /// Switch threshold on the yellow wire (V). UI shows / writes this via
    /// the slider; persisted per-tuner via `tuner{1,2}_threshold_v`.
    pub threshold_v: f32,
    /// Hysteresis around the threshold (V). Persisted per-tuner via
    /// `tuner{1,2}_hysteresis_v`.
    pub hysteresis_v: f32,
    /// Computed active edge: yellow_v < this counts as tune-active.
    /// Clamped to physically-reachable range `[0, ADC_VREF * DIVIDER_RATIO]`
    /// so an out-of-range threshold/hysteresis combination is visible to the
    /// operator instead of silently producing a never-triggerable edge.
    pub threshold_active_v: f32,
    /// Computed idle edge: yellow_v > this counts as tune-idle. Same
    /// clamping as `threshold_active_v`.
    pub threshold_idle_v: f32,
    /// `true` when the requested edges fell outside the reachable yellow
    /// range so the UI can warn the operator that the slider combo will never
    /// actually trigger (e.g. threshold 0.5 V + hysteresis 2.0 V -> requested
    /// active edge at -0.5 V, unreachable).
    pub edges_clamped: bool,
}

struct Inner {
    device: Option<MCP2221>,
    status: Status,
    /// Target board's USB serial number. `None` falls back to "first available
    /// board" (legacy single-tuner behaviour). `Some(serial)` opens via
    /// `connect_with_serial` so two boards plugged in at once can be
    /// discriminated.
    target_serial: Option<String>,
    gp2_high: bool,
    last_adc_raw: Option<u16>,
    last_adc_at: Option<Instant>,
    threshold_v: f32,
    hysteresis_v: f32,
}

impl Inner {
    fn new(target_serial: Option<String>) -> Self {
        Self {
            device: None,
            status: Status::NotInitialized,
            target_serial,
            gp2_high: false,
            last_adc_raw: None,
            last_adc_at: None,
            threshold_v: DEFAULT_THRESHOLD_V,
            hysteresis_v: DEFAULT_HYSTERESIS_V,
        }
    }

    /// Try to (re)open the MCP2221A and configure pin modes.
    /// On success transitions to `Status::Connected`; otherwise records
    /// the error and keeps `device = None`.
    fn try_connect(&mut self) {
        let open_result = match &self.target_serial {
            Some(sn) if !sn.is_empty() => MCP2221::connect_with_serial(sn),
            _ => MCP2221::connect(),
        };
        let dev = match open_result {
            Ok(d) => d,
            Err(e) => {
                self.device = None;
                self.status = Status::Error(format!("connect: {:?}", e));
                return;
            }
        };
        // Configure GP1 = ADC, GP2 = digital out (low). GP0/GP3 untouched.
        let mut gp = match dev.sram_read_settings() {
            Ok((_chip, gp)) => gp,
            Err(e) => {
                self.device = None;
                self.status = Status::Error(format!("sram_read: {:?}", e));
                return;
            }
        };
        gp.gp1_mode = Gp1Mode::AnalogInput;
        gp.gp2_mode = Gp2Mode::Gpio;
        gp.gp2_direction = GpioDirection::Output;
        gp.gp2_value = LogicLevel::Low;
        if let Err(e) = dev.sram_write_gp_settings(gp) {
            self.device = None;
            self.status = Status::Error(format!("sram_write: {:?}", e));
            return;
        }
        self.device = Some(dev);
        self.status = Status::Connected;
        self.gp2_high = false;
    }
}

/// Helper: raw ADC value -> yellow-wire voltage (post-divider).
fn raw_to_yellow_v(raw: u16) -> f32 {
    (raw as f32) * ADC_VREF / ADC_FULL_SCALE * DIVIDER_RATIO
}

/// Shared MCP2221A bridge state - cloned into [`TunerInstance`] and read by
/// the UI each frame.
pub struct Mcp2221Debug {
    inner: Mutex<Inner>,
}

impl Mcp2221Debug {
    /// New bridge bound to a specific USB serial number. Pass `None` to fall
    /// back to "first available board" - only sensible with a single tuner.
    pub fn with_target_serial(target_serial: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner::new(target_serial)),
        })
    }

    /// Force a (re)connect attempt. Called from the per-tuner init and from
    /// the tune sequence when the bridge is found disconnected.
    pub fn reconnect(&self) {
        let mut g = self.inner.lock().expect("mcp bridge mutex poisoned");
        // Drop any existing handle first so the HID device gets released
        // cleanly before reopen.
        g.device = None;
        g.try_connect();
    }

    /// Drive GP2 to the given level. Records the new state regardless of
    /// success; if the device is missing it tries to reconnect first.
    pub fn set_gp2(&self, high: bool) {
        let mut g = self.inner.lock().expect("mcp bridge mutex poisoned");
        if g.device.is_none() {
            g.try_connect();
        }
        if let Some(dev) = g.device.as_ref() {
            let mut changes = GpioChanges::new();
            changes.with_gp2_level(if high { LogicLevel::High } else { LogicLevel::Low });
            match dev.gpio_write(&changes) {
                Ok(_) => {
                    g.gp2_high = high;
                    g.status = Status::Connected;
                }
                Err(e) => {
                    g.device = None;
                    g.status = Status::Error(format!("gpio_write: {:?}", e));
                }
            }
        }
    }

    /// Poll GP1 ADC if the rate-limit window has elapsed; otherwise just
    /// return the last sample. Called from the UI render path and the
    /// tuner thread.
    pub fn snapshot(&self) -> DebugSnapshot {
        let mut g = self.inner.lock().expect("mcp bridge mutex poisoned");

        // Rate-limit USB traffic; skip the poll if too soon since last.
        let should_poll = match g.last_adc_at {
            None => g.device.is_some(),
            Some(t) => g.device.is_some() && t.elapsed() >= Duration::from_millis(ADC_POLL_MIN_MS),
        };
        if should_poll {
            if let Some(dev) = g.device.as_ref() {
                match dev.analog_read() {
                    Ok(r) => {
                        if let Some(raw) = r.gp1 {
                            g.last_adc_raw = Some(raw);
                            g.last_adc_at = Some(Instant::now());
                            g.status = Status::Connected;
                        }
                    }
                    Err(e) => {
                        g.device = None;
                        g.status = Status::Error(format!("analog_read: {:?}", e));
                    }
                }
            }
        }

        let last_adc_raw = g.last_adc_raw;
        let last_yellow_v = last_adc_raw.map(raw_to_yellow_v);
        let last_adc_at = g.last_adc_at;
        let last_adc_age = last_adc_at.map(|t| t.elapsed());
        let max_yellow = ADC_VREF * DIVIDER_RATIO;
        let half_hyst = g.hysteresis_v * 0.5;
        let requested_active = g.threshold_v - half_hyst;
        let requested_idle = g.threshold_v + half_hyst;
        let threshold_active_v = requested_active.clamp(0.0, max_yellow);
        let threshold_idle_v = requested_idle.clamp(0.0, max_yellow);
        let edges_clamped =
            requested_active < 0.0 || requested_idle > max_yellow;
        DebugSnapshot {
            status: g.status.clone(),
            gp2_high: g.gp2_high,
            last_adc_raw,
            last_yellow_v,
            last_adc_age,
            last_adc_at,
            threshold_v: g.threshold_v,
            hysteresis_v: g.hysteresis_v,
            threshold_active_v,
            threshold_idle_v,
            edges_clamped,
        }
    }

    /// Set the switch threshold on the yellow wire (volts). Clamped to
    /// [0.5, 4.5] so the slider can't accidentally produce a value the ADC
    /// could never reach (Vref * divider_ratio = 6.6 V at full scale, but
    /// the JC-Control idle level is ~4.5 V and never higher).
    pub fn set_threshold_v(&self, v: f32) {
        let mut g = self.inner.lock().expect("mcp bridge mutex poisoned");
        g.threshold_v = v.clamp(0.5, 4.5);
    }

    /// Set the hysteresis around the threshold (volts). Clamped to
    /// [0.1, 2.0] so the active/idle window is always non-degenerate.
    pub fn set_hysteresis_v(&self, v: f32) {
        let mut g = self.inner.lock().expect("mcp bridge mutex poisoned");
        g.hysteresis_v = v.clamp(0.1, 2.0);
    }

    /// True when `raw` looks like the tune-LED is on (yellow voltage below
    /// the clamped active edge - see `DebugSnapshot::threshold_active_v`).
    pub fn is_tune_active(&self, raw: u16) -> bool {
        let g = self.inner.lock().expect("mcp bridge mutex poisoned");
        let max_yellow = ADC_VREF * DIVIDER_RATIO;
        let active_edge = (g.threshold_v - g.hysteresis_v * 0.5).clamp(0.0, max_yellow);
        raw_to_yellow_v(raw) < active_edge
    }

    /// True when `raw` looks like the tune-LED is off (yellow voltage above
    /// the clamped idle edge - see `DebugSnapshot::threshold_idle_v`).
    pub fn is_tune_idle(&self, raw: u16) -> bool {
        let g = self.inner.lock().expect("mcp bridge mutex poisoned");
        let max_yellow = ADC_VREF * DIVIDER_RATIO;
        let idle_edge = (g.threshold_v + g.hysteresis_v * 0.5).clamp(0.0, max_yellow);
        raw_to_yellow_v(raw) > idle_edge
    }
}
