// SPDX-License-Identifier: GPL-2.0-or-later

//! MCP2221A-based driver for the Yaesu G-1000DXC rotor (driven
//! directly via an Adafruit MCP2221A breakout at 5 V, without an
//! intermediate EA7HG PCB).
//!
//! Hardware mapping (see memory `project_yaesu_rotor_mcp2221_plan`):
//!
//! | MCP2221A pin | Function | DIN-7 pin Yaesu | Notes |
//! |--------------|---------|-----------------|-------------|
//! | GP0 (GPIO out) | gate BST82 (CW) | drain -> pin 1 (R/CW) | low = idle, high = CW active |
//! | GP1 (GPIO out) | gate BST82 (CCW) | drain -> pin 2 (L/CCW) | low = idle, high = CCW active |
//! | GP2 (DAC) | speed | pin 3 (Speed) | DAC-Vref = Vdd -> 0-5 V, 5-bit (32 steps) |
//! | GP3 (ADC) | position feedback | pin 4 (Position) via 1.8k+2.2k divider | ADC-Vref = internal 4.096 V, 10-bit (max reading ≈ 7.45 V) |
//! | GND | common ground | pin 5 (Signal GND) | |
//!
//! Important: the breakout must first be jumpered to 5 V (3V pad
//! cut, 5V pad bridged on the underside of the Adafruit board);
//! otherwise the BST82 does not switch hard enough and the DAC range is
//! only 0-3.3 V.
//!
//! This module contains only low-level primitives + an initialization
//! routine that sets the board's SRAM settings to the correct mode.
//! Server integration (RotorBackend, EquipmentCommand handlers, polling
//! thread) follows in phase 3 of PATCH-yaesu-rotor-mcp2221.

use anyhow::{anyhow, Context, Result};
use log::{debug, info, warn};
use std::collections::VecDeque;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::rotor::{Rotor, RotorCmd, RotorStatus};

use mcp2221_hal::analog::{VoltageReference, VrmVoltage};
use mcp2221_hal::gpio::{GpioChanges, GpioDirection, LogicLevel};
use mcp2221_hal::settings::{Gp0Mode, Gp1Mode, Gp2Mode, Gp3Mode};
use mcp2221_hal::MCP2221;

/// DAC output at 5 bits (32 steps). With DAC-Vref = Vdd (5 V) that
/// yields ~156 mV per step. For the Yaesu G-1000DXC speed input that is
/// coarse but sufficient - the input is high-impedance (~108 k) and
/// intended for continuously-variable DC.
pub const DAC_MAX: u8 = 31;

/// 10-bit ADC full-scale.
pub const ADC_FULL_SCALE: u16 = 1023;

/// ADC reference voltage (internal Vrm 4.096 V). Not Vdd, because the
/// Adafruit USB rail varies and the Yaesu position output is
/// ratiometric relative to its own internal ~4.54 V reference. The
/// more stable internal 4.096 V gives reproducible calibration.
pub const ADC_VREF_V: f32 = 4.096;

/// Position voltage divider on the small board. After an initial
/// build (1.8 k + 10 k -> ratio 1.18, max reading 4.83 V) the operator
/// tightened it to 1.8 k + 2.2 k -> ratio (1800+2200)/2200 = 1.818, max
/// reading 4.096 × 1.818 ≈ 7.45 V. Reason: the Yaesu G-1000DXC can go
/// above 4.8 V on pin 4 for the highest degrees, which caused the ADC
/// to clip above ~365° with the first ratio. With 2.2 k on the bottom,
/// 0-5 V on pin 4 falls neatly within the ADC range with ample margin.
/// After this hardware change the operator must press "Park CCW" +
/// "Park CW" again because the stored v_at_0deg / v_at_max_deg values
/// are tied to the old ratio and otherwise no longer match the physical
/// position.
pub const POSITION_DIVIDER_RATIO: f32 = (1_800.0 + 2_200.0) / 2_200.0;

/// Driver status - mirrored on the `Mcp2221Debug` tuner path.
#[derive(Debug, Clone)]
pub enum Status {
    /// No attempt to connect made yet.
    NotInitialized,
    /// Board open + pin modes configured.
    Connected,
    /// Open failed or a later call returned an error. String contains the reason.
    Error(String),
}

/// Number of samples in the moving-average buffer for ADC filtering.
/// At 5 Hz poll = 2 sec window. Long enough to average out 50/60 Hz
/// ripple and USB jitter, short enough to follow rotor movement during
/// a run smoothly (~0.2°/sample × 10 = 2° after full settle).
pub const ADC_AVG_WINDOW: usize = 10;

/// Driver snapshot for UI and logging purposes.
#[derive(Debug, Clone)]
pub struct RotorSnapshot {
    pub status: Status,
    /// Last commanded GP0 level (CW): true = active.
    pub gp0_cw_high: bool,
    /// Last commanded GP1 level (CCW): true = active.
    pub gp1_ccw_high: bool,
    /// Last commanded DAC value (0..=DAC_MAX).
    pub dac_value: u8,
    /// Last read ADC raw counts (0..=ADC_FULL_SCALE), `None` until
    /// the first successful poll.
    pub last_adc_raw: Option<u16>,
    /// Number of samples currently in the moving-average buffer (0..=ADC_AVG_WINDOW).
    pub samples_in_window: usize,
    /// Mean ADC counts over the moving-average buffer.
    /// Still used for the log diagnostics; the UI shows `median_adc_raw`
    /// as the primary (more stable) value.
    pub avg_adc_raw: Option<f32>,
    /// Median ADC counts over the buffer. Robust against noise spikes;
    /// the UI shows this as the primary position value.
    pub median_adc_raw: Option<u16>,
    /// Min/max ADC counts in the buffer (for noise diagnostics). Both `None`
    /// until the first sample. Use (max - min) as the raw peak-to-peak
    /// spread.
    pub min_adc_raw: Option<u16>,
    pub max_adc_raw: Option<u16>,
}

struct Inner {
    device: Option<MCP2221>,
    status: Status,
    /// USB serial of the board to open (`rot_<naam>`). `None` falls
    /// back to "first board on the bus" - only useful for the
    /// standalone test-spike binary.
    target_serial: Option<String>,
    gp0_cw_high: bool,
    gp1_ccw_high: bool,
    dac_value: u8,
    last_adc_raw: Option<u16>,
    /// Ring buffer of the latest ADC samples for moving-average filtering.
    /// Previously the UI got a raw sample every 200 ms and the operator saw
    /// up to ~500 mV fluctuation on a DC feedback signal; this buffer
    /// averages that out to a more stable display value and at the same
    /// time provides min/max spread for noise diagnostics.
    adc_samples: VecDeque<u16>,
    /// Current capacity of the moving-average buffer. Adjusted by the
    /// poll thread based on motion/idle state: during movement the
    /// window shrinks to ~1 sec (30 samples), when idle it grows to
    /// ~60 sec (60 samples).
    adc_buffer_cap: usize,
}

impl Inner {
    fn new(target_serial: Option<String>) -> Self {
        Self {
            device: None,
            status: Status::NotInitialized,
            target_serial,
            gp0_cw_high: false,
            gp1_ccw_high: false,
            dac_value: 0,
            last_adc_raw: None,
            adc_samples: VecDeque::with_capacity(ADC_AVG_WINDOW),
            adc_buffer_cap: ADC_AVG_WINDOW,
        }
    }

    /// (Re)open the board and configure pin modes + analog references.
    fn try_connect(&mut self) -> Result<()> {
        let dev = match &self.target_serial {
            Some(sn) if !sn.is_empty() => MCP2221::connect_with_serial(sn)
                .map_err(|e| anyhow!("connect_with_serial({}): {:?}", sn, e))?,
            _ => MCP2221::connect().map_err(|e| anyhow!("connect: {:?}", e))?,
        };

        // GP0/GP1 = GPIO out (low = idle), GP2 = DAC, GP3 = ADC.
        let (_chip, mut gp) = dev
            .sram_read_settings()
            .map_err(|e| anyhow!("sram_read_settings: {:?}", e))?;
        gp.gp0_mode = Gp0Mode::Gpio;
        gp.gp0_direction = GpioDirection::Output;
        gp.gp0_value = LogicLevel::Low;
        gp.gp1_mode = Gp1Mode::Gpio;
        gp.gp1_direction = GpioDirection::Output;
        gp.gp1_value = LogicLevel::Low;
        gp.gp2_mode = Gp2Mode::AnalogOutput; // DAC1
        gp.gp3_mode = Gp3Mode::AnalogInput; // ADC3
        dev.sram_write_gp_settings(gp)
            .map_err(|e| anyhow!("sram_write_gp_settings: {:?}", e))?;

        // DAC-Vref = Vdd (5 V), ADC-Vref = internal 4.096 V.
        dev.analog_set_output_reference(VoltageReference::Vdd)
            .map_err(|e| anyhow!("analog_set_output_reference(Vdd): {:?}", e))?;
        dev.analog_set_input_reference(VoltageReference::Vrm(VrmVoltage::V4_096))
            .map_err(|e| anyhow!("analog_set_input_reference(Vrm 4.096): {:?}", e))?;

        // DAC starts at 0 (no speed).
        dev.analog_write(0)
            .map_err(|e| anyhow!("analog_write(0): {:?}", e))?;

        self.device = Some(dev);
        self.status = Status::Connected;
        self.gp0_cw_high = false;
        self.gp1_ccw_high = false;
        self.dac_value = 0;
        info!(
            "YaesuRotor: board connected + configured (target={:?})",
            self.target_serial
        );
        Ok(())
    }
}

/// Driver instance. Holds the USB handle and serializes all calls.
pub struct YaesuRotorDriver {
    inner: Mutex<Inner>,
}

impl YaesuRotorDriver {
    /// Create a new driver instance bound to a specific MCP2221A
    /// USB serial (typically `rot_<naam>` per ThetisLink convention). `None`
    /// falls back to "first board on the bus" - only sensible for one-off
    /// hardware tests; production must always go through a serial.
    pub fn with_target_serial(target_serial: Option<String>) -> Self {
        Self {
            inner: Mutex::new(Inner::new(target_serial)),
        }
    }

    /// Force a (re)connect attempt. Standard usage: at init and
    /// after a USB error.
    pub fn reconnect(&self) -> Result<()> {
        let mut g = self.inner.lock().expect("YaesuRotorDriver mutex poisoned");
        g.device = None;
        g.try_connect().context("reconnect")
    }

    /// Snapshot of the current driver state (without a USB call).
    /// Computes both mean (for log diagnostics) and median (for UI display)
    /// over the moving-average buffer. The median is robust against noise
    /// spikes that pull the mean away - operator finding 2026-06-04: EMI
    /// pickup causes spreads of ~150 raw counts regardless of rotor
    /// position, a median over 10 samples filters the outliers out
    /// effectively while rotor movements still track immediately.
    pub fn snapshot(&self) -> RotorSnapshot {
        let g = self.inner.lock().expect("YaesuRotorDriver mutex poisoned");
        let samples_in_window = g.adc_samples.len();
        let (avg_adc_raw, median_adc_raw, min_adc_raw, max_adc_raw) =
            if g.adc_samples.is_empty() {
                (None, None, None, None)
            } else {
                let sum: u32 = g.adc_samples.iter().map(|&v| v as u32).sum();
                let avg = sum as f32 / samples_in_window as f32;
                let min = g.adc_samples.iter().copied().min();
                let max = g.adc_samples.iter().copied().max();
                let mut sorted: Vec<u16> = g.adc_samples.iter().copied().collect();
                sorted.sort_unstable();
                let median = sorted[sorted.len() / 2];
                (Some(avg), Some(median), min, max)
            };
        RotorSnapshot {
            status: g.status.clone(),
            gp0_cw_high: g.gp0_cw_high,
            gp1_ccw_high: g.gp1_ccw_high,
            dac_value: g.dac_value,
            last_adc_raw: g.last_adc_raw,
            samples_in_window,
            avg_adc_raw,
            median_adc_raw,
            min_adc_raw,
            max_adc_raw,
        }
    }

    /// Write GP0 (CW) + GP1 (CCW) in a single HID transaction.
    ///
    /// Important: never drives both high at the same time (the Yaesu
    /// inputs are not electrically protected against simultaneous R+L).
    /// With `cw=true, ccw=true` CCW is automatically set to false and
    /// a warning is logged.
    pub fn set_direction(&self, cw: bool, ccw: bool) -> Result<()> {
        let (cw, ccw) = if cw && ccw {
            warn!("YaesuRotor: CW+CCW requested at the same time; CCW forced off");
            (true, false)
        } else {
            (cw, ccw)
        };

        let mut g = self.inner.lock().expect("YaesuRotorDriver mutex poisoned");
        if g.device.is_none() {
            g.try_connect().context("auto-reconnect in set_direction")?;
        }
        let dev = g
            .device
            .as_ref()
            .ok_or_else(|| anyhow!("device niet beschikbaar"))?;

        let mut changes = GpioChanges::new();
        changes.with_gp0_level(if cw { LogicLevel::High } else { LogicLevel::Low });
        changes.with_gp1_level(if ccw { LogicLevel::High } else { LogicLevel::Low });
        dev.gpio_write(&changes).map_err(|e| {
            let msg = format!("gpio_write CW={} CCW={}: {:?}", cw, ccw, e);
            g.device = None;
            g.status = Status::Error(msg.clone());
            anyhow!(msg)
        })?;
        g.gp0_cw_high = cw;
        g.gp1_ccw_high = ccw;
        g.status = Status::Connected;
        // At debug level: during GoTo, set_direction is called again
        // every tick (~30 Hz) to hold CW/CCW; an info log per tick gave
        // unreadable spam in the server log.
        debug!("YaesuRotor: GP0(CW)={} GP1(CCW)={}", cw, ccw);
        Ok(())
    }

    /// Set the DAC value (0..=`DAC_MAX`). Higher value = faster rotor
    /// (the Yaesu input is high-impedance and accepts 0-5 V continuously).
    pub fn set_dac(&self, value: u8) -> Result<()> {
        let value = value.min(DAC_MAX);
        let mut g = self.inner.lock().expect("YaesuRotorDriver mutex poisoned");
        if g.device.is_none() {
            g.try_connect().context("auto-reconnect in set_dac")?;
        }
        let dev = g
            .device
            .as_ref()
            .ok_or_else(|| anyhow!("device niet beschikbaar"))?;
        dev.analog_write(value).map_err(|e| {
            let msg = format!("analog_write({}): {:?}", value, e);
            g.device = None;
            g.status = Status::Error(msg.clone());
            anyhow!(msg)
        })?;
        g.dac_value = value;
        debug!(
            "YaesuRotor: DAC={} ({}/{} = {:.2} V @ Vdd≈5V)",
            value,
            value,
            DAC_MAX,
            (value as f32 / DAC_MAX as f32) * 5.0
        );
        Ok(())
    }

    /// Read the ADC on GP3 (position feedback) and return raw counts.
    /// No rate limit here - the caller (poll thread or spike binary)
    /// determines the cadence itself.
    pub fn read_position_raw(&self) -> Result<u16> {
        let mut g = self.inner.lock().expect("YaesuRotorDriver mutex poisoned");
        if g.device.is_none() {
            g.try_connect().context("auto-reconnect in read_position_raw")?;
        }
        let dev = g
            .device
            .as_ref()
            .ok_or_else(|| anyhow!("device niet beschikbaar"))?;
        let reading = dev.analog_read().map_err(|e| {
            let msg = format!("analog_read: {:?}", e);
            g.device = None;
            g.status = Status::Error(msg.clone());
            anyhow!(msg)
        })?;
        let raw = reading
            .gp3
            .ok_or_else(|| anyhow!("GP3 niet als ADC geconfigureerd"))?;
        g.last_adc_raw = Some(raw);
        // Push into the moving-average ring buffer; drop oldest sample(s)
        // until we are below the current `adc_buffer_cap`. The cap can
        // shrink dynamically (motion->idle transition sets a smaller cap)
        // so possibly drop several old samples in one call.
        let cap = g.adc_buffer_cap.max(1);
        while g.adc_samples.len() >= cap {
            g.adc_samples.pop_front();
        }
        g.adc_samples.push_back(raw);
        Ok(raw)
    }

    /// Adjust the moving-average buffer size (motion vs idle).
    /// When shrinking, the oldest samples are dropped; when growing,
    /// existing samples remain and the buffer fills slowly.
    pub fn set_buffer_cap(&self, cap: usize) {
        let cap = cap.max(1);
        let mut g = self.inner.lock().expect("YaesuRotorDriver mutex poisoned");
        g.adc_buffer_cap = cap;
        while g.adc_samples.len() > cap {
            g.adc_samples.pop_front();
        }
    }

    /// Clear all samples from the moving-average buffer. Used on the
    /// idle->motion transition so that old 1-Hz samples do not pollute
    /// the display during an active rotation.
    pub fn clear_samples(&self) {
        let mut g = self.inner.lock().expect("YaesuRotorDriver mutex poisoned");
        g.adc_samples.clear();
    }

    /// Convert raw ADC counts to the Yaesu position-pin voltage,
    /// i.e. undoing the small board's voltage divider.
    pub fn raw_to_yaesu_volts(raw: u16) -> f32 {
        let adc_v = (raw as f32) * ADC_VREF_V / (ADC_FULL_SCALE as f32);
        adc_v * POSITION_DIVIDER_RATIO
    }
}

// ── Server-side runtime: RotorInstance + poll-thread ─────────────────────

/// Poll interval during active movement (gate on or GoTo target).
/// 33 ms ≈ 30 Hz; deliberately **not** a multiple of 50/60 Hz so that
/// samples do not sync with the 50/100 Hz mains-supply ripple that
/// leaks onto the Yaesu position output. Window = 30 samples = 1 sec average.
const ADC_POLL_INTERVAL_MOTION_MS: u64 = 33;

/// Poll interval when idle (gate off, no target active). 1 Hz +
/// 60-sample buffer = 60-sec moving average, for a very calm UI
/// display. On transition to movement the buffer is brought back
/// in-flight to the motion cap.
const ADC_POLL_INTERVAL_IDLE_MS: u64 = 1000;

/// Buffer size during movement. Kept deliberately small (10 samples
/// × 33 ms ≈ 333 ms window, median lag ~165 ms) so that the control
/// loop sees the actual rotor position fast enough to engage the
/// soft-stop in time. With 30 samples the measured position lagged
/// ~1.5° (at 3°/s) and the decel trigger came too late -
/// the rotor stopped abruptly at target instead of linearly.
const ADC_AVG_WINDOW_MOTION: usize = 10;

/// Buffer size when idle (60 samples × 1000 ms = 60 sec).
const ADC_AVG_WINDOW_IDLE: usize = 60;

/// How many degrees between current position and target must still
/// remain before we stop the movement (anti-overshoot deadband).
/// Conservatively at 1°; the Yaesu's own mechanical slop is ~1° so
/// software cannot be more precise.
const GOTO_DEADBAND_DEG: f32 = 1.0;

/// Estimated rotor speed at max DAC (deg/sec). Used to estimate the
/// decel distance for soft-stop so that the rotor lands exactly on
/// target at min DAC. The G-1000DXC at DAC ≈ 5 V reaches about
/// 6°/s; the previously set 3°/s gave too short a decel zone.
const MAX_ROTOR_DEG_PER_SEC: f32 = 6.0;

/// Safety factor on the computed decel distance. The actual rotor
/// speed varies (mast load, supply droop) and the median filter gives
/// a small measurement lag anyway - better to start ramp-down well in
/// time and let the last degrees run out at DAC=0 than an abrupt stop
/// at target.
const DECEL_SAFETY_FACTOR: f32 = 2.2;

/// Live snapshot for the UI: contains both command state and the last
/// measured ADC position.
#[derive(Debug, Clone)]
pub struct RotorInstanceStatus {
    pub status: Status,
    /// Display label (`name` field from RotorConfig, or mcp_serial if
    /// name is empty). The UI uses this for the panel title.
    pub label: String,
    pub gp0_cw_high: bool,
    pub gp1_ccw_high: bool,
    pub dac_value: u8,
    /// Last read raw ADC counts (0..=1023), `None` until the first
    /// successful poll.
    pub last_adc_raw: Option<u16>,
    /// Same, converted to the Yaesu position-pin voltage (post
    /// small-board voltage divider).
    pub last_yaesu_volts: Option<f32>,
    /// Number of samples currently in the moving-average buffer.
    pub samples_in_window: usize,
    /// Mean ADC counts over the buffer (for log diagnostics only).
    pub avg_adc_raw: Option<f32>,
    /// Median ADC counts (primary for UI; robust against noise spikes).
    pub median_adc_raw: Option<u16>,
    /// Yaesu-pin voltage derived from `median_adc_raw`. The UI shows this
    /// as the primary voltage value - no drift from outliers.
    pub median_yaesu_volts: Option<f32>,
    /// Peak-to-peak spread of the samples in the window (max − min,
    /// in raw counts). 0 = perfectly stable; high = noise on the source.
    pub adc_p2p_raw: Option<u16>,
    /// Current calibration state (endpoints + max_deg). The UI renders
    /// the Park buttons + max_deg spinner based on this.
    pub calibration: RotorCalibration,
    /// Computed position in degrees via linear mapping of
    /// `median_yaesu_volts` over the calibration endpoints. `None`
    /// if the calibration is invalid (endpoints equal) or no sample yet.
    pub position_deg: Option<f32>,
}

/// Calibration state per rotor: linear mapping of Yaesu pin-4
/// voltage to degrees. Adjusted by the UI buttons "Park CCW"/"Park CW"
/// + max_deg spinner; persisted via `config::modify_config`.
#[derive(Debug, Clone, Copy)]
pub struct RotorCalibration {
    pub v_at_0deg: f32,
    pub v_at_max_deg: f32,
    pub max_deg: u16,
    /// Ramp rate for soft-start/stop (% per second).
    pub ramp_pct_per_sec: f32,
    /// For rotors with overlap (max_deg > 360): on GoTo, choose the
    /// shortest route via the overlap zone. See `RotorConfig` doc.
    pub shortest_route_in_overlap: bool,
}

impl Default for RotorCalibration {
    fn default() -> Self {
        Self {
            v_at_0deg: 0.0,
            v_at_max_deg: 4.5,
            max_deg: 450,
            ramp_pct_per_sec: 50.0,
            shortest_route_in_overlap: false,
        }
    }
}

impl RotorCalibration {
    /// Convert pin-4 voltage to rotor position in degrees via
    /// linear mapping. Returns `None` if the calibration is still
    /// invalid (endpoints too close together / inverted).
    pub fn volts_to_degrees(&self, v: f32) -> Option<f32> {
        let span = self.v_at_max_deg - self.v_at_0deg;
        if span.abs() < 0.01 {
            // Endpoints equal -> not calibrated, no mapping possible.
            return None;
        }
        let frac = (v - self.v_at_0deg) / span;
        Some(frac * self.max_deg as f32)
    }
}

/// Server-side wrapper around `YaesuRotorDriver` with a background poll
/// thread that reads the ADC continuously, so the UI can show a live
/// position display without making USB calls itself. Also a
/// `Rotor`-facade producer so that the existing client rotor window
/// (position display, CW/CCW/Stop/GoTo) works automatically without
/// client-side changes.
pub struct RotorInstance {
    driver: Arc<YaesuRotorDriver>,
    label: String,
    /// Slot index in `config.rotors` so that `set_calibration_*` knows
    /// which entry to mutate. For MAX_ROTORS=1 now always 0.
    slot_index: usize,
    /// Calibration state in memory - mirrored with the `config.rotors[slot]`
    /// calibration fields. The UI mutates here (Park buttons), the poll
    /// thread reads here (for GoTo control). `Arc<Mutex<>>` so both
    /// share the same shared state.
    calibration: Arc<Mutex<RotorCalibration>>,
    /// Cmd channel for the `Rotor` facade (client -> server commands).
    /// The poll thread handles one command per tick.
    cmd_tx: mpsc::Sender<RotorCmd>,
    /// Live `RotorStatus` - shared with the `Rotor` facade. The poll
    /// thread writes the current angle/connected/rotating here on every ADC poll.
    rotor_status: Arc<Mutex<RotorStatus>>,
    /// Shutdown flag for the poll thread. No explicit JoinHandle -
    /// the rotor instance normally lives for the entire server runtime.
    shutdown: Arc<std::sync::atomic::AtomicBool>,
}

impl RotorInstance {
    /// Open the board (by USB serial) and spawn the ADC poll thread.
    /// Fails softly: if the board cannot be opened initially, the poll
    /// thread still runs and will periodically try to reconnect. The UI
    /// sees `Status::Error(..)` until the board is available.
    pub fn new(
        slot_index: usize,
        serial: &str,
        label: &str,
        calibration: RotorCalibration,
    ) -> Arc<Self> {
        let target = if serial.is_empty() {
            None
        } else {
            Some(serial.to_string())
        };
        let driver = Arc::new(YaesuRotorDriver::with_target_serial(target));
        // Initial reconnect attempt: log the result but do not block the start.
        if let Err(e) = driver.reconnect() {
            warn!(
                "RotorInstance {}: init reconnect failed ({:?}); poll-thread retried periodically",
                label, e
            );
        }
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (cmd_tx, cmd_rx) = mpsc::channel::<RotorCmd>();
        let rotor_status = Arc::new(Mutex::new(RotorStatus::default()));
        let calibration_arc = Arc::new(Mutex::new(calibration));
        let inst = Arc::new(Self {
            driver: driver.clone(),
            label: label.to_string(),
            slot_index,
            calibration: calibration_arc.clone(),
            cmd_tx,
            rotor_status: rotor_status.clone(),
            shutdown: shutdown.clone(),
        });

        // Spawn poll thread: reads the ADC every ADC_POLL_INTERVAL_MS,
        // updates the driver-internal `last_adc_raw`, writes the current
        // angle to `rotor_status`, and handles incoming RotorCmd's
        // (manual Cw/Ccw/Stop + bang-bang GoTo control loop).
        let label_for_thread = label.to_string();
        std::thread::Builder::new()
            .name(format!("rotor-poll-{}", label))
            .spawn(move || {
                rotor_poll_thread(
                    driver,
                    shutdown,
                    label_for_thread,
                    cmd_rx,
                    rotor_status,
                    calibration_arc,
                );
            })
            .ok();
        info!("RotorInstance {}: started", label);
        inst
    }

    /// Return a `Rotor` facade that supports the existing client rotor
    /// protocol (CW/CCW/Stop/GoTo + RotorStatus poll).
    /// The caller can place the same Rotor in `ServerState.rotor`
    /// as would otherwise be an EA7HG/PstRotator instance.
    pub fn make_rotor_facade(&self) -> Rotor {
        // Pass the actual max rotation through the facade so that external
        // input sources (PstRotator listener) can consider the
        // mech-target+360° as an alternative for overlap rotors.
        let max_deg = self
            .calibration
            .lock()
            .map(|c| c.max_deg)
            .unwrap_or(450);
        let max_deg_x10 = (max_deg as u16).saturating_mul(10);
        Rotor::from_handles_with_max(
            self.cmd_tx.clone(),
            self.rotor_status.clone(),
            max_deg_x10,
        )
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// Live snapshot of driver + label for UI rendering.
    pub fn status(&self) -> RotorInstanceStatus {
        let snap = self.driver.snapshot();
        let last_yaesu_volts = snap
            .last_adc_raw
            .map(|raw| YaesuRotorDriver::raw_to_yaesu_volts(raw));
        let median_yaesu_volts = snap
            .median_adc_raw
            .map(|raw| YaesuRotorDriver::raw_to_yaesu_volts(raw));
        let adc_p2p_raw = match (snap.min_adc_raw, snap.max_adc_raw) {
            (Some(lo), Some(hi)) => Some(hi.saturating_sub(lo)),
            _ => None,
        };
        let calibration = *self.calibration.lock().expect("rotor cal mutex poisoned");
        let position_deg =
            median_yaesu_volts.and_then(|v| calibration.volts_to_degrees(v));
        RotorInstanceStatus {
            status: snap.status,
            label: self.label.clone(),
            gp0_cw_high: snap.gp0_cw_high,
            gp1_ccw_high: snap.gp1_ccw_high,
            dac_value: snap.dac_value,
            last_adc_raw: snap.last_adc_raw,
            last_yaesu_volts,
            samples_in_window: snap.samples_in_window,
            avg_adc_raw: snap.avg_adc_raw,
            median_adc_raw: snap.median_adc_raw,
            median_yaesu_volts,
            adc_p2p_raw,
            calibration,
            position_deg,
        }
    }

    /// Store the current median voltage as the 0° endpoint
    /// (CCW park). Persists via `config::modify_config`.
    pub fn park_ccw(&self) {
        let snap = self.driver.snapshot();
        let v = match snap.median_adc_raw {
            Some(raw) => YaesuRotorDriver::raw_to_yaesu_volts(raw),
            None => {
                warn!("RotorInstance {}: park_ccw without ADC sample", self.label);
                return;
            }
        };
        let mut cal = self.calibration.lock().expect("rotor cal mutex poisoned");
        cal.v_at_0deg = v;
        let cal_copy = *cal;
        drop(cal);
        self.persist_calibration(cal_copy);
        info!(
            "RotorInstance {}: park CCW set to {:.3} V (raw median {})",
            self.label,
            v,
            snap.median_adc_raw.unwrap()
        );
    }

    /// Store the current median voltage as the max° endpoint
    /// (CW park).
    pub fn park_cw(&self) {
        let snap = self.driver.snapshot();
        let v = match snap.median_adc_raw {
            Some(raw) => YaesuRotorDriver::raw_to_yaesu_volts(raw),
            None => {
                warn!("RotorInstance {}: park_cw without ADC sample", self.label);
                return;
            }
        };
        let mut cal = self.calibration.lock().expect("rotor cal mutex poisoned");
        cal.v_at_max_deg = v;
        let cal_copy = *cal;
        drop(cal);
        self.persist_calibration(cal_copy);
        info!(
            "RotorInstance {}: park CW set to {:.3} V (raw median {})",
            self.label,
            v,
            snap.median_adc_raw.unwrap()
        );
    }

    /// Update `max_deg` (UI spinner). Default 450 for the G-1000DXC.
    pub fn set_max_deg(&self, max_deg: u16) {
        let max_deg = max_deg.clamp(90, 720);
        let mut cal = self.calibration.lock().expect("rotor cal mutex poisoned");
        cal.max_deg = max_deg;
        let cal_copy = *cal;
        drop(cal);
        self.persist_calibration(cal_copy);
        info!("RotorInstance {}: max_deg set to {}°", self.label, max_deg);
    }

    /// Toggle the "shortest route via overlap zone" option. Persisted.
    pub fn set_shortest_route_in_overlap(&self, on: bool) {
        let mut cal = self.calibration.lock().expect("rotor cal mutex poisoned");
        cal.shortest_route_in_overlap = on;
        let cal_copy = *cal;
        drop(cal);
        self.persist_calibration(cal_copy);
        info!(
            "RotorInstance {}: shortest_route_in_overlap = {}",
            self.label, on
        );
    }

    /// Update soft-start/stop ramp rate (% per second). Persisted.
    pub fn set_ramp_pct_per_sec(&self, pct: f32) {
        let pct = pct.clamp(1.0, 200.0);
        let mut cal = self.calibration.lock().expect("rotor cal mutex poisoned");
        cal.ramp_pct_per_sec = pct;
        let cal_copy = *cal;
        drop(cal);
        self.persist_calibration(cal_copy);
        info!("RotorInstance {}: ramp_pct_per_sec set to {:.1}", self.label, pct);
    }

    fn persist_calibration(&self, cal: RotorCalibration) {
        let slot = self.slot_index;
        crate::config::modify_config(|c| {
            if let Some(r) = c.rotors.get_mut(slot) {
                r.v_at_0deg = cal.v_at_0deg;
                r.v_at_max_deg = cal.v_at_max_deg;
                r.max_deg = cal.max_deg;
                r.ramp_pct_per_sec = cal.ramp_pct_per_sec;
                r.shortest_route_in_overlap = cal.shortest_route_in_overlap;
            }
        });
    }

    /// CW/CCW command - delegates directly to the driver (USB call on
    /// the calling thread, same pattern as Mcp2221Debug). Best
    /// from an action handler (button click) - not from a
    /// high-frequency render loop.
    pub fn set_direction(&self, cw: bool, ccw: bool) {
        if let Err(e) = self.driver.set_direction(cw, ccw) {
            warn!("RotorInstance {}: set_direction failed: {:?}", self.label, e);
        }
    }

    /// DAC speed command.
    pub fn set_dac(&self, value: u8) {
        if let Err(e) = self.driver.set_dac(value) {
            warn!("RotorInstance {}: set_dac({}) failed: {:?}", self.label, value, e);
        }
    }
}

impl Drop for RotorInstance {
    fn drop(&mut self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // Best-effort cleanup: GP0/1 LOW + DAC 0 so that a hot-swap
        // does not leave the rotor in an active state.
        let _ = self.driver.set_direction(false, false);
        let _ = self.driver.set_dac(0);
        info!("RotorInstance {}: dropped (shutdown signaled)", self.label);
    }
}

fn rotor_poll_thread(
    driver: Arc<YaesuRotorDriver>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    label: String,
    cmd_rx: mpsc::Receiver<RotorCmd>,
    rotor_status: Arc<Mutex<RotorStatus>>,
    calibration: Arc<Mutex<RotorCalibration>>,
) {
    let mut consecutive_errors = 0usize;
    let mut last_stat_log = Instant::now();
    let stat_log_interval = Duration::from_secs(5);
    // Active GoTo target, in degrees. `None` means no automatic
    // movement - manual cmds (Cw/Ccw) or idle.
    let mut target_deg: Option<f32> = None;
    // Ramp state: current DAC output (float for smoothness) + where we
    // are ramping to. On gate-on or GoTo -> dac_target = DAC_MAX; on
    // landing or stop -> dac_target = 0. Each tick `current_dac`
    // interpolates by `ramp_pct_per_sec`.
    let mut current_dac: f32 = 0.0;
    let mut dac_target: f32 = 0.0;
    // Previous tick timestamp for the exact ramp-step computation (actual
    // elapsed time, not the nominal tick - otherwise we lose ramp speed
    // if the OS stretches the sleep time).
    let mut last_tick = Instant::now();
    // Previous motion state for edge detection: on idle->motion we clear
    // the buffer (start fresh at 30 Hz), on motion->idle we leave the
    // samples in place and the buffer grows slowly to 60.
    let mut was_in_motion = false;
    // Manual mode: as soon as an external source (server-UI test buttons
    // or speed slider) changes the DAC instead of the ramp loop, the
    // poll thread hands off DAC control so the slider value stays put.
    // It is cleared as soon as a cmd-channel command comes in
    // (Cw/Ccw/Stop/GoTo via the client). Detection via `last_written_dac`:
    // if the actual driver DAC no longer matches what we last wrote,
    // someone else has intervened.
    let mut manual_mode = false;
    let mut last_written_dac: Option<u8> = None;
    // Initially the cap is at ADC_AVG_WINDOW (10); on the first tick we
    // force the correct motion/idle cap.
    driver.set_buffer_cap(ADC_AVG_WINDOW_IDLE);
    info!(
        "rotor-poll {}: thread started (motion={} ms / idle={} ms tick)",
        label, ADC_POLL_INTERVAL_MOTION_MS, ADC_POLL_INTERVAL_IDLE_MS
    );
    loop {
        if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        match driver.read_position_raw() {
            Ok(_raw) => {
                consecutive_errors = 0;
            }
            Err(e) => {
                consecutive_errors += 1;
                // Log the first error in detail, then stay quiet until
                // it recovers (otherwise 5 Hz log spam).
                if consecutive_errors == 1 {
                    warn!("rotor-poll {}: read_position_raw failed: {:?}", label, e);
                }
                // Every 50 polls (10 sec) a retry-reconnect attempt.
                if consecutive_errors % 50 == 0 {
                    if let Err(e2) = driver.reconnect() {
                        debug!("rotor-poll {}: reconnect retry failed: {:?}", label, e2);
                    } else {
                        info!("rotor-poll {}: reconnected after {} errors", label, consecutive_errors);
                        consecutive_errors = 0;
                    }
                }
            }
        }
        // Live RotorStatus update: compute the current degrees from the
        // median voltage + active calibration and publish in the
        // shared `rotor_status` Arc so the client rotor window
        // sees it without its own USB call.
        let cal_snap = *calibration.lock().expect("rotor cal mutex poisoned");
        let drv_snap = driver.snapshot();
        let median_v = drv_snap
            .median_adc_raw
            .map(|raw| YaesuRotorDriver::raw_to_yaesu_volts(raw));
        let current_deg = median_v.and_then(|v| cal_snap.volts_to_degrees(v));
        {
            let mut st = rotor_status.lock().expect("rotor status mutex poisoned");
            st.connected = matches!(drv_snap.status, Status::Connected);
            // angle_x10 expects 0..3600 for 0..360°. The Yaesu G-1000DXC
            // goes up to 450°; we clamp to 0..max_deg and multiply.
            // Should the client UI be strictly 0..360, it now sees a
            // wider range - no harm, and owners who want to show the
            // extra 90° benefit from it.
            if let Some(d) = current_deg {
                let clamped = d.clamp(0.0, cal_snap.max_deg as f32);
                st.angle_x10 = (clamped * 10.0).round() as u16;
            }
            st.rotating = drv_snap.gp0_cw_high || drv_snap.gp1_ccw_high;
            // target_x10 is only filled when a GoTo is active.
            st.target_x10 = target_deg
                .map(|t| (t.clamp(0.0, cal_snap.max_deg as f32) * 10.0).round() as u16)
                .unwrap_or(0);
        }

        // Pull pending rotor commands (manual + GoTo). Non-blocking.
        // Manual cmds set dac_target = max -> soft-start ramp up.
        // Stop sets dac_target = 0 + gate off (no ramp-down needed on an
        // explicit stop). GoTo sets target_deg and lets the control loop
        // handle ramp + landing afterwards.
        match cmd_rx.try_recv() {
            Ok(RotorCmd::Stop) => {
                target_deg = None;
                dac_target = 0.0;
                current_dac = 0.0;
                manual_mode = false;
                if let Err(e) = driver.set_direction(false, false) {
                    warn!("rotor-poll {}: Stop set_direction failed: {:?}", label, e);
                }
                let _ = driver.set_dac(0);
                last_written_dac = Some(0);
                info!("rotor-poll {}: Stop", label);
            }
            Ok(RotorCmd::Cw) => {
                target_deg = None;
                dac_target = DAC_MAX as f32;
                manual_mode = false;
                if let Err(e) = driver.set_direction(true, false) {
                    warn!("rotor-poll {}: Cw set_direction failed: {:?}", label, e);
                }
                info!("rotor-poll {}: manual CW (ramp to DAC {})", label, DAC_MAX);
            }
            Ok(RotorCmd::Ccw) => {
                target_deg = None;
                dac_target = DAC_MAX as f32;
                manual_mode = false;
                if let Err(e) = driver.set_direction(false, true) {
                    warn!("rotor-poll {}: Ccw set_direction failed: {:?}", label, e);
                }
                info!("rotor-poll {}: manual CCW (ramp to DAC {})", label, DAC_MAX);
            }
            Ok(RotorCmd::GoTo(x10)) => {
                manual_mode = false;
                let t = (x10 as f32) / 10.0;
                let primary = t.clamp(0.0, cal_snap.max_deg as f32);
                // Shortest-route option: for rotors with an overlap zone
                // (max_deg > 360) an alternative target = primary
                // ± 360° can yield physically the same antenna direction,
                // but via a shorter mechanical route. Operator choice
                // 2026-06-04: per-rotor checkbox, default off.
                let chosen = if cal_snap.shortest_route_in_overlap
                    && cal_snap.max_deg > 360
                {
                    let cur = current_deg.unwrap_or(primary);
                    let max_d = cal_snap.max_deg as f32;
                    let mut candidates: Vec<f32> = vec![primary];
                    // Alternative +360 (valid if <= max_deg)
                    if primary + 360.0 <= max_d {
                        candidates.push(primary + 360.0);
                    }
                    // Alternative -360 (valid if >= 0)
                    if primary - 360.0 >= 0.0 {
                        candidates.push(primary - 360.0);
                    }
                    let best = candidates
                        .into_iter()
                        .min_by(|a, b| {
                            (a - cur)
                                .abs()
                                .partial_cmp(&(b - cur).abs())
                                .unwrap_or(std::cmp::Ordering::Equal)
                        })
                        .unwrap_or(primary);
                    if (best - primary).abs() > 0.01 {
                        info!(
                            "rotor-poll {}: shortest-route chosen {:.1}° (instead of {:.1}°, current {:.1}°)",
                            label, best, primary, cur
                        );
                    }
                    best
                } else {
                    primary
                };
                target_deg = Some(chosen);
                dac_target = DAC_MAX as f32;
                info!("rotor-poll {}: GoTo target {:.1}°", label, chosen);
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                info!("rotor-poll {}: cmd-channel closed, keeps running for poll", label);
            }
        }

        // GoTo control loop with soft-stop landing. Compute how many
        // degrees the rotor still needs to reach the target and how
        // many degrees it would cover during a ramp-down based on the
        // current DAC + ramp_pct_per_sec. If distance-to-target ≤
        // decel-distance -> start ramp-down (dac_target=0). At
        // |Δ| ≤ deadband -> gate off, clear target.
        if let (Some(target), Some(current)) = (target_deg, current_deg) {
            let delta = target - current;
            if delta.abs() <= GOTO_DEADBAND_DEG {
                if let Err(e) = driver.set_direction(false, false) {
                    warn!("rotor-poll {}: target-reached stop failed: {:?}", label, e);
                }
                dac_target = 0.0;
                current_dac = 0.0;
                let _ = driver.set_dac(0);
                info!(
                    "rotor-poll {}: GoTo target reached ({:.1}° vs {:.1}°, Δ={:.2})",
                    label, current, target, delta
                );
                target_deg = None;
            } else {
                // Direction-reversal protection: a running GoTo that
                // gets a new target on the other side during movement
                // (delta sign flips) previously had to switch the CW/CCW
                // gates immediately while current_dac was still at MAX -
                // an abrupt motor reversal at full power.
                // Detect here whether the desired direction differs from
                // the current gate state; if so and current_dac > 0,
                // hold the current direction and first ramp the DAC to 0.
                // Only when the DAC is ~0 do we switch the gates and start
                // the ramp-up in the new direction.
                let desired_cw = delta > 0.0;
                let direction_mismatch =
                    (desired_cw && drv_snap.gp1_ccw_high)
                        || (!desired_cw && drv_snap.gp0_cw_high);
                if direction_mismatch && current_dac > 1.0 {
                    // Soft-stop phase: gates stay as they were,
                    // dac_target to 0. On the next tick (or a few
                    // ticks depending on the ramp) current_dac drops
                    // below the threshold and falls into the else branch
                    // with the correct gates.
                    dac_target = 0.0;
                } else {
                    // Safe to set direction: either no mismatch (the
                    // rotor is already facing the right way), or the
                    // DAC is already ~0.
                    let _ = driver.set_direction(desired_cw, !desired_cw);
                    // Decel distance: at ramp_pct_per_sec, current_dac goes
                    // from the current fraction to 0 in (frac × 100 / pct) sec.
                    // Average speed during ramp = MAX × frac / 2.
                    // Distance = avg_speed × time = MAX × frac² × 50 / pct.
                    // Plus `DECEL_SAFETY_FACTOR` to cover measurement lag +
                    // speed uncertainty - better to brake too early than too late.
                    let frac = current_dac / DAC_MAX as f32;
                    let decel_time_sec =
                        frac * 100.0 / cal_snap.ramp_pct_per_sec.max(1.0);
                    let decel_dist = MAX_ROTOR_DEG_PER_SEC
                        * frac
                        * 0.5
                        * decel_time_sec
                        * DECEL_SAFETY_FACTOR;
                    if delta.abs() <= decel_dist.max(GOTO_DEADBAND_DEG) {
                        // Start ramp-down to 0 so that we end within the
                        // deadband at min DAC.
                        dac_target = 0.0;
                    } else {
                        dac_target = DAC_MAX as f32;
                    }
                }
            }
        }

        // Detect manual override: if the actual driver DAC does not
        // match what we last wrote, someone else has intervened
        // (server-UI speed slider or test buttons).
        // We respect that value and hand off DAC control until
        // a new client cmd comes in.
        let actual_dac = drv_snap.dac_value;
        if let Some(last) = last_written_dac {
            if actual_dac != last && !manual_mode {
                debug!(
                    "rotor-poll {}: external DAC override ({} -> {}), entering manual mode",
                    label, last, actual_dac
                );
                manual_mode = true;
                current_dac = actual_dac as f32;
                dac_target = actual_dac as f32;
                // Abort a running GoTo - the user has taken over
                // control. Clear the target angle so the ramp loop does
                // not try to reach it again later.
                target_deg = None;
            }
        }

        let now = Instant::now();
        let dt_sec = (now - last_tick).as_secs_f32();
        last_tick = now;

        if !manual_mode {
            // Normal ramp: interpolate `current_dac` towards
            // `dac_target` at `ramp_pct_per_sec`. Actual elapsed time
            // since the previous tick (OS-jitter compensation).
            let dac_step =
                cal_snap.ramp_pct_per_sec * 0.01 * DAC_MAX as f32 * dt_sec;
            if (dac_target - current_dac).abs() <= dac_step {
                current_dac = dac_target;
            } else if dac_target > current_dac {
                current_dac += dac_step;
            } else {
                current_dac -= dac_step;
            }
            current_dac = current_dac.clamp(0.0, DAC_MAX as f32);
            let new_dac = current_dac.round() as u8;
            // Only write if the value actually changed - otherwise
            // during idle we keep doing a no-op USB call every tick and
            // run the risk of accidentally overwriting a manual DAC
            // between ticks.
            if last_written_dac.map(|p| p != new_dac).unwrap_or(true) {
                let _ = driver.set_dac(new_dac);
                last_written_dac = Some(new_dac);
            }
        } else {
            // Manual mode: keep `current_dac` in sync with the actual
            // DAC so that a subsequent cmd-channel command ramps on from
            // the current level instead of from 0.
            current_dac = actual_dac as f32;
            last_written_dac = Some(actual_dac);
        }

        // Determine motion state *after* command handling + ramp step:
        // the rotor is "in motion" if a gate is on OR the DAC is not
        // at 0 OR a GoTo target is active. On the transition to
        // motion: the buffer cap shrinks to 30 and the buffer is
        // cleared so the 30-Hz window starts fresh. On the transition to
        // idle: the cap grows to 60; existing samples remain.
        let in_motion = drv_snap.gp0_cw_high
            || drv_snap.gp1_ccw_high
            || target_deg.is_some()
            || current_dac > 0.5
            || dac_target > 0.5;
        if in_motion != was_in_motion {
            if in_motion {
                driver.set_buffer_cap(ADC_AVG_WINDOW_MOTION);
                driver.clear_samples();
                info!(
                    "rotor-poll {}: -> MOTION (cap={}, tick={} ms, buffer cleared)",
                    label, ADC_AVG_WINDOW_MOTION, ADC_POLL_INTERVAL_MOTION_MS
                );
            } else {
                driver.set_buffer_cap(ADC_AVG_WINDOW_IDLE);
                info!(
                    "rotor-poll {}: -> IDLE (cap={}, tick={} ms)",
                    label, ADC_AVG_WINDOW_IDLE, ADC_POLL_INTERVAL_IDLE_MS
                );
            }
            was_in_motion = in_motion;
        }

        // Periodic ADC statistics (raw counts min/max/mean/median +
        // converted pin4 voltages). Was used during hardware tuning
        // to diagnose the noise level; now that the hardware is
        // stable this is at `debug!` so it no longer pollutes the
        // server log but stays available via
        // `RUST_LOG=sdr_remote_server::mcp2221_yaesu_rotor=debug`.
        if last_stat_log.elapsed() >= stat_log_interval {
            let snap = driver.snapshot();
            if let (Some(avg), Some(median), Some(min), Some(max)) = (
                snap.avg_adc_raw,
                snap.median_adc_raw,
                snap.min_adc_raw,
                snap.max_adc_raw,
            ) {
                let to_v = |raw_f: f32| {
                    raw_f * ADC_VREF_V / (ADC_FULL_SCALE as f32) * POSITION_DIVIDER_RATIO
                };
                debug!(
                    "rotor-poll {} ADC[{}]: raw min={} max={} mean={:.1} median={} (Δ={}) ; pin4 V min={:.3} max={:.3} median={:.3} (Δ={:.3} V)",
                    label,
                    snap.samples_in_window,
                    min,
                    max,
                    avg,
                    median,
                    max.saturating_sub(min),
                    to_v(min as f32),
                    to_v(max as f32),
                    to_v(median as f32),
                    to_v(max as f32) - to_v(min as f32),
                );
            }
            last_stat_log = Instant::now();
        }
        let sleep_ms = if was_in_motion {
            ADC_POLL_INTERVAL_MOTION_MS
        } else {
            ADC_POLL_INTERVAL_IDLE_MS
        };
        std::thread::sleep(Duration::from_millis(sleep_ms));
    }
    info!("rotor-poll {}: thread stopped", label);
}
