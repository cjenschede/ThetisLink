// SPDX-License-Identifier: GPL-2.0-or-later

//! UI-observability contract for the unified control rendering.
//!
//! Design principles:
//!
//! - Zero-cost when observability is off (prod default): `tracing::enabled!`
//!   short-circuit in `TracingSink`, no allocations per event.
//! - Events go through the intent layer; every `cmd_tx.send` from a control-helper
//!   MUST be preceded by `record_intent` + guard-check - enforced
//!   by `ControlContext::cmd_tx` being private (only `dispatch()` can send).
//! - `RecordingSink` is only available under `cfg(test)` or
//!   `feature = "ui-test"` - not in release builds.
//! - All events get a `frame_id` + `t_mono_ns` stamp at emit time (see
//!   `StampedEvent`) for timeline correlation in jq scripts and
//!   intent-chain asserts.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use super::{RxChannel, UiDensity, UiSurface};

static NEXT_INTENT_ID: AtomicU64 = AtomicU64::new(1);
static CURRENT_FRAME: AtomicU64 = AtomicU64::new(0);
static MONO_START: OnceLock<Instant> = OnceLock::new();

pub(crate) type IntentId = u64;

pub(crate) fn next_intent_id() -> IntentId {
    NEXT_INTENT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Increment the frame-id. Called once per render-frame by the
/// render-orchestrator (step 2).
pub(crate) fn begin_frame() -> u64 {
    CURRENT_FRAME.fetch_add(1, Ordering::Relaxed) + 1
}

/// Current frame-id. 0 before the first `begin_frame()` call.
pub(crate) fn current_frame() -> u64 {
    CURRENT_FRAME.load(Ordering::Relaxed)
}

/// Monotonic time since the first observability emit, in nanoseconds.
/// Cheap: one `Instant::now()` + one subtraction.
pub(crate) fn mono_ns_since_start() -> u64 {
    let start = MONO_START.get_or_init(Instant::now);
    start.elapsed().as_nanos() as u64
}

/// All UI actions that go through the intent layer. Stays limited to
/// control-helpers; audio/PTT/connection-init stay direct on `cmd_tx`
/// (hot-path, own latency rules).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum UiIntent {
    /// Tune the current frequency by a delta (Hz). Used for both the
    /// `−`/`+` step-buttons and the scroll-wheel. Source distinction stays
    /// visible via the preceding `UiEvent::ScrollTuneApplied` (scroll) or
    /// `UiEvent::ClickReceived` on `freq_step_arrows` (button).
    TuneByDelta { channel: RxChannel, delta_hz: i64 },
    SelectBand { channel: RxChannel, band_hz: u64 },
    SelectMode { channel: RxChannel, mode: u8 },
    VfoSwap { channel: RxChannel },
    VfoSync,
    /// User typed an absolute frequency in the inline-edit and
    /// submitted with Enter. The only channel for absolute freq-set from
    /// a control-helper - memory-recall or other absolute-freq features
    /// go later via a new intent variant if they have control-helper
    /// origin.
    InlineFreqEdit { channel: RxChannel, hz: u64 },
}

impl UiIntent {
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            UiIntent::TuneByDelta { .. } => "tune_by_delta",
            UiIntent::SelectBand { .. } => "select_band",
            UiIntent::SelectMode { .. } => "select_mode",
            UiIntent::VfoSwap { .. } => "vfo_swap",
            UiIntent::VfoSync => "vfo_sync",
            UiIntent::InlineFreqEdit { .. } => "inline_freq_edit",
        }
    }
}

/// Reason why an intent was not converted into a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandBlockReason {
    Disconnected,
    RateLimited,
}

impl CommandBlockReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            CommandBlockReason::Disconnected => "disconnected",
            CommandBlockReason::RateLimited => "rate_limited",
        }
    }
}

/// Structured events that the observability layer emits.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum UiEvent {
    /// N.B. This event is NOT emitted from render-helpers - helpers are
    /// stateless and cannot detect an enabled-transition without an extra
    /// per-site tracker. Plan:
    /// move to an app-level `ConnectionStateChanged` emit when
    /// `self.connected` actually changes value. Variant stays
    /// for forward-compat; no emitter in this phase.
    GuardTransition {
        control_id: &'static str,
        channel: RxChannel,
        surface: UiSurface,
        density: UiDensity,
        now_enabled: bool,
    },
    ClickReceived {
        control_id: &'static str,
        channel: RxChannel,
        surface: UiSurface,
        density: UiDensity,
        was_enabled: bool,
    },
    IntentEmitted {
        intent: UiIntent,
        connected: bool,
        intent_id: IntentId,
    },
    CommandSent {
        intent_kind: &'static str,
        connected: bool,
        intent_id: IntentId,
    },
    CommandBlocked {
        intent_kind: &'static str,
        reason: CommandBlockReason,
        intent_id: IntentId,
    },
    /// Detected when `cmd_tx.send` fails (channel closed).
    /// Distinguishes hard from `CommandSent` to prevent false positives.
    CommandSendFailed {
        intent_kind: &'static str,
        intent_id: IntentId,
    },
    ScrollTuneApplied {
        channel: RxChannel,
        delta_hz: i64,
        connected: bool,
    },
    InlineFreqSubmitted {
        channel: RxChannel,
        hz: u64,
        connected: bool,
    },
    /// Non-production instrumentation only; never emitted in prod (see
    /// `TracingSink::emit`). Uses a separate tracing-target `ui::frame`
    /// so that `RUST_LOG` can filter them independently of other ui-events.
    RenderFrame {
        surface: UiSurface,
        control_count: u32,
        guarded_count: u32,
    },
}

/// Stamped event - what `RecordingSink` holds and what log-parsers
/// can correlate via `frame_id` and `t_mono_ns`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StampedEvent {
    pub(crate) frame_id: u64,
    pub(crate) t_mono_ns: u64,
    pub(crate) event: UiEvent,
}

/// Sink-contract. `emit` must be zero-cost in the prod default via a
/// `tracing::enabled!` check.
pub(crate) trait UiEventSink: Send + Sync {
    fn emit(&self, event: UiEvent);
    fn record_intent(&self, intent: &UiIntent, connected: bool) -> IntentId;
}

/// Prod implementation: routes to `tracing` with structured fields.
pub(crate) struct TracingSink;

impl TracingSink {
    #[inline]
    fn stamp_fields() -> (u64, u64) {
        (current_frame(), mono_ns_since_start())
    }
}

impl UiEventSink for TracingSink {
    fn emit(&self, event: UiEvent) {
        // Short-circuit when nothing is listening - no field assembly, no allocation.
        if !tracing::enabled!(target: "ui", tracing::Level::INFO) {
            // RenderFrame goes on a separate target; check separately.
            if !matches!(event, UiEvent::RenderFrame { .. }) {
                return;
            }
            if !tracing::enabled!(target: "ui::frame", tracing::Level::INFO) {
                return;
            }
        }
        let (frame_id, t_mono_ns) = Self::stamp_fields();
        match event {
            UiEvent::GuardTransition {
                control_id,
                channel,
                surface,
                density,
                now_enabled,
            } => {
                tracing::info!(
                    target: "ui",
                    event = "guard_transition",
                    frame_id,
                    t_mono_ns,
                    control = control_id,
                    channel = channel.as_str(),
                    surface = surface.as_str(),
                    density = density.as_str(),
                    now_enabled,
                );
            }
            UiEvent::ClickReceived {
                control_id,
                channel,
                surface,
                density,
                was_enabled,
            } => {
                tracing::info!(
                    target: "ui",
                    event = "click_received",
                    frame_id,
                    t_mono_ns,
                    control = control_id,
                    channel = channel.as_str(),
                    surface = surface.as_str(),
                    density = density.as_str(),
                    was_enabled,
                );
            }
            UiEvent::IntentEmitted {
                intent,
                connected,
                intent_id,
            } => {
                tracing::info!(
                    target: "ui",
                    event = "intent_emitted",
                    frame_id,
                    t_mono_ns,
                    intent = intent.kind(),
                    connected,
                    intent_id,
                );
            }
            UiEvent::CommandSent {
                intent_kind,
                connected,
                intent_id,
            } => {
                tracing::info!(
                    target: "ui",
                    event = "command_sent",
                    frame_id,
                    t_mono_ns,
                    intent = intent_kind,
                    connected,
                    intent_id,
                );
            }
            UiEvent::CommandBlocked {
                intent_kind,
                reason,
                intent_id,
            } => {
                tracing::info!(
                    target: "ui",
                    event = "command_blocked",
                    frame_id,
                    t_mono_ns,
                    intent = intent_kind,
                    reason = reason.as_str(),
                    intent_id,
                );
            }
            UiEvent::CommandSendFailed {
                intent_kind,
                intent_id,
            } => {
                tracing::info!(
                    target: "ui",
                    event = "command_send_failed",
                    frame_id,
                    t_mono_ns,
                    intent = intent_kind,
                    intent_id,
                );
            }
            UiEvent::ScrollTuneApplied {
                channel,
                delta_hz,
                connected,
            } => {
                tracing::info!(
                    target: "ui",
                    event = "scroll_tune_applied",
                    frame_id,
                    t_mono_ns,
                    channel = channel.as_str(),
                    delta_hz,
                    connected,
                );
            }
            UiEvent::InlineFreqSubmitted {
                channel,
                hz,
                connected,
            } => {
                tracing::info!(
                    target: "ui",
                    event = "inline_freq_submitted",
                    frame_id,
                    t_mono_ns,
                    channel = channel.as_str(),
                    hz,
                    connected,
                );
            }
            UiEvent::RenderFrame {
                surface,
                control_count,
                guarded_count,
            } => {
                // RenderFrame uses non-production instrumentation + separate target.
                #[cfg(any(test, feature = "ui-test"))]
                {
                    tracing::info!(
                        target: "ui::frame",
                        event = "render_frame",
                        frame_id,
                        t_mono_ns,
                        surface = surface.as_str(),
                        control_count,
                        guarded_count,
                    );
                }
                #[cfg(not(any(test, feature = "ui-test")))]
                {
                    let _ = (surface, control_count, guarded_count);
                }
            }
        }
    }

    fn record_intent(&self, intent: &UiIntent, connected: bool) -> IntentId {
        let id = next_intent_id();
        self.emit(UiEvent::IntentEmitted {
            intent: intent.clone(),
            connected,
            intent_id: id,
        });
        id
    }
}

// ---------------------------------------------------------------------------
// RecordingSink - only under test or feature = "ui-test". No stub in prod
// builds: the symbol does not exist, so it cannot be accidentally
// constructed.
// ---------------------------------------------------------------------------

#[cfg(any(test, feature = "ui-test"))]
pub(crate) struct RecordingSink {
    inner: std::sync::Mutex<Vec<StampedEvent>>,
}

#[cfg(any(test, feature = "ui-test"))]
impl RecordingSink {
    pub(crate) fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// All recorded events including frame_id + t_mono_ns stamp.
    pub(crate) fn stamped(&self) -> Vec<StampedEvent> {
        self.inner.lock().unwrap().clone()
    }

    /// Events without stamp - handy for PartialEq-based asserts.
    pub(crate) fn events(&self) -> Vec<UiEvent> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.event.clone())
            .collect()
    }

    pub(crate) fn clear(&self) {
        self.inner.lock().unwrap().clear();
    }

    /// Number of events for which `pred` is true.
    pub(crate) fn count_by<F: Fn(&UiEvent) -> bool>(&self, pred: F) -> usize {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .filter(|s| pred(&s.event))
            .count()
    }

    /// First event for which `pred` is true.
    pub(crate) fn find<F: Fn(&UiEvent) -> bool>(&self, pred: F) -> Option<UiEvent> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .find(|s| pred(&s.event))
            .map(|s| s.event.clone())
    }

    /// Verify that a given `intent_id` closes its chain cleanly:
    /// one `IntentEmitted` + exactly one of `CommandSent` / `CommandBlocked` /
    /// `CommandSendFailed`, no duplicates.
    pub(crate) fn assert_intent_chain(&self, id: IntentId) -> Result<(), String> {
        let events = self.inner.lock().unwrap();
        let mut emitted = 0usize;
        let mut terminal = 0usize;
        for stamped in events.iter() {
            match &stamped.event {
                UiEvent::IntentEmitted { intent_id, .. } if *intent_id == id => emitted += 1,
                UiEvent::CommandSent { intent_id, .. } if *intent_id == id => terminal += 1,
                UiEvent::CommandBlocked { intent_id, .. } if *intent_id == id => terminal += 1,
                UiEvent::CommandSendFailed { intent_id, .. } if *intent_id == id => terminal += 1,
                _ => {}
            }
        }
        if emitted != 1 {
            return Err(format!(
                "intent_id {id}: verwacht 1 IntentEmitted, kreeg {emitted}"
            ));
        }
        if terminal != 1 {
            return Err(format!(
                "intent_id {id}: verwacht 1 terminal (Sent/Blocked/Failed), kreeg {terminal}"
            ));
        }
        Ok(())
    }
}

#[cfg(any(test, feature = "ui-test"))]
impl UiEventSink for RecordingSink {
    fn emit(&self, event: UiEvent) {
        let stamped = StampedEvent {
            frame_id: current_frame(),
            t_mono_ns: mono_ns_since_start(),
            event,
        };
        self.inner.lock().unwrap().push(stamped);
    }

    fn record_intent(&self, intent: &UiIntent, connected: bool) -> IntentId {
        let id = next_intent_id();
        self.emit(UiEvent::IntentEmitted {
            intent: intent.clone(),
            connected,
            intent_id: id,
        });
        id
    }
}
