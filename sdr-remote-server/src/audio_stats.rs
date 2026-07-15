// SPDX-License-Identifier: GPL-2.0-or-later

//! Lock-free audio-activity counters for the Status panel (PATCH-2).
//!
//! Per the brief: the audio-callback / encoder loops MUST NEVER block on
//! a Mutex or RwLock. Activity is reported via `AtomicU64` + `AtomicI64`
//! only - `fetch_add(1, Relaxed)` plus `store(now_nanos, Relaxed)` is
//! branch-free and allocation-free.
//!
//! UI reads with `load(Relaxed)` and computes `active = now - last < 1s`
//! itself. `last_frame_at_nanos == 0` means "never seen" - UI shows "-".

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// One channel's running activity counters.
#[derive(Debug, Default)]
pub struct ChannelStats {
    /// Cumulative frame count since server start.
    pub frame_count: AtomicU64,
    /// Monotonic nanoseconds-since-server-start of the last frame.
    /// 0 = never seen (server has not yet observed a frame on this channel).
    pub last_frame_at_nanos: AtomicI64,
}

impl ChannelStats {
    /// Hot-path: record one frame. Two relaxed atomic writes - no locks,
    /// no allocations, no branches on the happy path.
    pub fn tick(&self, server_start: Instant) {
        self.frame_count.fetch_add(1, Ordering::Relaxed);
        let ns = server_start.elapsed().as_nanos() as i64;
        self.last_frame_at_nanos.store(ns, Ordering::Relaxed);
    }

    /// UI-side snapshot: (frame_count, last_frame_age_secs_or_none).
    /// Returns `None` for `last_seen` if the channel has never seen a frame.
    pub fn snapshot(&self, server_start: Instant) -> (u64, Option<f32>) {
        let count = self.frame_count.load(Ordering::Relaxed);
        let last_ns = self.last_frame_at_nanos.load(Ordering::Relaxed);
        let age = if last_ns == 0 {
            None
        } else {
            let now_ns = server_start.elapsed().as_nanos() as i64;
            Some(((now_ns - last_ns) as f64 / 1e9) as f32)
        };
        (count, age)
    }
}

/// All audio channels the server speaks. Wrapped in `Arc<>` so the audio
/// loops, network RX loop, and the UI all share the same atomics.
#[derive(Debug, Default)]
pub struct AudioActivityStats {
    /// Thetis RX1 -> client (main receive slice).
    pub rx1: ChannelStats,
    /// Thetis RX2 -> client (second slice when enabled).
    pub rx2: ChannelStats,
    /// Client -> Thetis TX (decoded mic audio written to CABLE-B).
    pub tx: ChannelStats,
    /// Yaesu USB -> client.
    pub yaesu_rx: ChannelStats,
    /// Client -> Yaesu USB (TX audio routed to Yaesu).
    pub yaesu_tx: ChannelStats,
}

impl AudioActivityStats {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

/// PATCH-2: lock-free probe for TCI connection state, shared with the UI
/// Status panel. Updated by `TciConnection` on every state-flip.
#[derive(Debug, Default)]
pub struct TciStatusProbe {
    /// Currently connected to Thetis via WebSocket?
    pub connected: std::sync::atomic::AtomicBool,
    /// Server-start nanoseconds at which `connected` last flipped.
    /// 0 = never observed.
    pub last_state_change_ns: AtomicI64,
}

/// Bundle of the lock-free Status-panel state shared between the server
/// runtime and the eframe UI. GUI mode constructs it once in `ServerApp`
/// and threads clones into `run_server_async`; the UI reads it directly.
/// CLI mode skips construction (no UI to render into).
#[derive(Clone)]
pub struct StatusPanelShared {
    pub audio: Arc<AudioActivityStats>,
    pub tci: Arc<TciStatusProbe>,
    /// Receiver for the SessionManager. `run_server_async` calls `set()`
    /// once after construction; UI reads via `get()`. Until set the UI
    /// shows "starting..." for client-related fields.
    pub session_slot: Arc<std::sync::OnceLock<Arc<tokio::sync::Mutex<crate::session::SessionManager>>>>,
    pub server_start: Instant,
    /// Actual bind outcome - published by `NetworkService::new` after the
    /// UDP socket bind attempt. UI renders the real result instead of the
    /// pre-bind default ("0.0.0.0:4580 starting..."), so a bind-fail is not
    /// papered over with a green-looking address line.
    pub bind_status: Arc<std::sync::OnceLock<BindStatus>>,
    /// Live `Tuners` collection - published once after `Tuners::new` so the
    /// Status panel can render per-tuner rows (Live yellow voltage,
    /// threshold + hysteresis sliders) without needing a separate channel.
    pub tuners_slot: Arc<std::sync::OnceLock<Arc<crate::tuner::Tuners>>>,
    /// Live Yaesu-rotor instance (PATCH-yaesu-rotor-mcp2221 fase 3).
    /// `None` zolang er geen `rot_*` MCP2221A in `config.rotors` zit,
    /// of zolang server-start nog niet de poll-thread heeft opgestart.
    pub rotor_slot: Arc<std::sync::OnceLock<Arc<crate::mcp2221_yaesu_rotor::RotorInstance>>>,
    /// Optional outbound relay monitor status (Phase A: status only, no TL frames routed).
    pub relay_status: Arc<std::sync::OnceLock<sdr_remote_relay::RelayStatusHandle>>,
}

/// Outcome of the UDP bind attempt - surfaced in the Status panel.
#[derive(Debug, Clone)]
pub enum BindStatus {
    Ok { addr: String },
    Failed { addr: String, error: String },
}

impl StatusPanelShared {
    pub fn new() -> Self {
        Self {
            audio: AudioActivityStats::new(),
            tci: TciStatusProbe::new(),
            session_slot: Arc::new(std::sync::OnceLock::new()),
            server_start: Instant::now(),
            bind_status: Arc::new(std::sync::OnceLock::new()),
            tuners_slot: Arc::new(std::sync::OnceLock::new()),
            rotor_slot: Arc::new(std::sync::OnceLock::new()),
            relay_status: Arc::new(std::sync::OnceLock::new()),
        }
    }
}

impl TciStatusProbe {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Idempotent: only stamps `last_state_change_ns` when the value
    /// actually flips. Safe to call every heartbeat with the current
    /// state - duration shown by the UI reflects the last real flip,
    /// not the last call.
    pub fn update(&self, connected: bool, server_start: Instant) {
        let prev = self.connected.swap(connected, Ordering::Relaxed);
        if prev != connected {
            self.last_state_change_ns
                .store(server_start.elapsed().as_nanos() as i64, Ordering::Relaxed);
        }
    }

    /// UI-side snapshot: (is_connected, age_secs_or_none).
    pub fn snapshot(&self, server_start: Instant) -> (bool, Option<f32>) {
        let connected = self.connected.load(Ordering::Relaxed);
        let last_ns = self.last_state_change_ns.load(Ordering::Relaxed);
        let age = if last_ns == 0 {
            None
        } else {
            let now_ns = server_start.elapsed().as_nanos() as i64;
            Some(((now_ns - last_ns) as f64 / 1e9) as f32)
        };
        (connected, age)
    }
}
