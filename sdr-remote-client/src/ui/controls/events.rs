// SPDX-License-Identifier: GPL-2.0-or-later

//! UI-observability contract voor de unified control rendering.
//!
//! Ontwerpprincipes:
//!
//! - Zero-cost als observability uit staat (prod default): `tracing::enabled!`
//!   short-circuit in `TracingSink`, geen allocaties per event.
//! - Events gaan door de intent-laag; elke `cmd_tx.send` uit een control-helper
//!   MOET voorafgegaan worden door `record_intent` + guard-check — afgedwongen
//!   doordat `ControlContext::cmd_tx` privé is (alleen `dispatch()` kan senden).
//! - `RecordingSink` is alleen beschikbaar onder `cfg(test)` of
//!   `feature = "ui-test"` — niet in release-builds.
//! - Alle events krijgen bij emit een `frame_id` + `t_mono_ns`-stempel mee (zie
//!   `StampedEvent`) voor timeline-correlatie in jq-scripts en
//!   intent-chain-asserts.

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

/// Hoog het frame-id op. Wordt één keer per render-frame door de
/// render-orchestrator aangeroepen (stap 2).
pub(crate) fn begin_frame() -> u64 {
    CURRENT_FRAME.fetch_add(1, Ordering::Relaxed) + 1
}

/// Huidig frame-id. 0 vóór de eerste `begin_frame()`-aanroep.
pub(crate) fn current_frame() -> u64 {
    CURRENT_FRAME.load(Ordering::Relaxed)
}

/// Monotone tijd sinds de eerste observability-emit, in nanoseconden.
/// Goedkoop: één `Instant::now()` + één subtraction.
pub(crate) fn mono_ns_since_start() -> u64 {
    let start = MONO_START.get_or_init(Instant::now);
    start.elapsed().as_nanos() as u64
}

/// Alle UI-acties die door de intent-laag gaan. Blijft beperkt tot
/// control-helpers; audio/PTT/connection-init blijven direct op `cmd_tx`
/// (hot-path, eigen latency-regels).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum UiIntent {
    /// Tune de huidige frequency met een delta (Hz). Gebruikt voor zowel de
    /// `−`/`+` step-buttons als het scroll-wheel. Source-onderscheid blijft
    /// zichtbaar via voorafgaande `UiEvent::ScrollTuneApplied` (scroll) of
    /// `UiEvent::ClickReceived` op `freq_step_arrows` (knop).
    TuneByDelta { channel: RxChannel, delta_hz: i64 },
    SelectBand { channel: RxChannel, band_hz: u64 },
    SelectMode { channel: RxChannel, mode: u8 },
    VfoSwap { channel: RxChannel },
    VfoSync,
    /// Gebruiker heeft een absolute frequency getypt in de inline-edit en
    /// ingediend met Enter. Het enige kanaal voor absolute freq-set vanuit
    /// een control-helper — memory-recall of andere absolute-freq features
    /// gaan later via een nieuwe intent-variant als ze control-helper
    /// oorsprong hebben.
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

/// Reden waarom een intent niet in een command is omgezet.
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

/// Structured events die de observability-laag uitstuurt.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum UiEvent {
    /// N.B. Dit event wordt NIET uit render-helpers geëmit — helpers zijn
    /// stateless en kunnen geen enabled-overgang detecteren zonder extra
    /// per-site tracker. Plan:
    /// verplaatsen naar een app-level `ConnectionStateChanged` emit wanneer
    /// `self.connected` daadwerkelijk van waarde verandert. Variant blijft
    /// voor forward-compat; geen emitter in deze fase.
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
    /// Gedetecteerd wanneer `cmd_tx.send` faalt (kanaal gesloten).
    /// Onderscheidt hard van `CommandSent` om vals-positieven te voorkomen.
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
    /// Alleen in test/ui-test builds emitted; in prod nooit (zie
    /// `TracingSink::emit`). Gebruikt een aparte tracing-target `ui::frame`
    /// zodat `RUST_LOG` ze onafhankelijk van andere ui-events kan filteren.
    RenderFrame {
        surface: UiSurface,
        control_count: u32,
        guarded_count: u32,
    },
}

/// Gestampeld event — wat `RecordingSink` vasthoudt en wat log-parsers
/// kunnen correleren via `frame_id` en `t_mono_ns`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StampedEvent {
    pub(crate) frame_id: u64,
    pub(crate) t_mono_ns: u64,
    pub(crate) event: UiEvent,
}

/// Sink-contract. `emit` moet in prod default zero-cost zijn via een
/// `tracing::enabled!` check.
pub(crate) trait UiEventSink: Send + Sync {
    fn emit(&self, event: UiEvent);
    fn record_intent(&self, intent: &UiIntent, connected: bool) -> IntentId;
}

/// Prod-implementatie: routeert naar `tracing` met structured fields.
pub(crate) struct TracingSink;

impl TracingSink {
    #[inline]
    fn stamp_fields() -> (u64, u64) {
        (current_frame(), mono_ns_since_start())
    }
}

impl UiEventSink for TracingSink {
    fn emit(&self, event: UiEvent) {
        // Short-circuit wanneer niets luistert — geen veld-assembly, geen allocatie.
        if !tracing::enabled!(target: "ui", tracing::Level::INFO) {
            // RenderFrame gaat op een apart target; check apart.
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
                // RenderFrame gaat alleen in test/ui-test builds + aparte target.
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
// RecordingSink — alleen onder test of feature = "ui-test". Geen stub in prod
// builds: het symbool bestaat niet, dus kan niet per ongeluk worden
// geconstrueerd.
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

    /// Alle opgenomen events inclusief frame_id + t_mono_ns stempel.
    pub(crate) fn stamped(&self) -> Vec<StampedEvent> {
        self.inner.lock().unwrap().clone()
    }

    /// Events zonder stempel — handig voor PartialEq-gebaseerde asserts.
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

    /// Aantal events waarvoor `pred` true is.
    pub(crate) fn count_by<F: Fn(&UiEvent) -> bool>(&self, pred: F) -> usize {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .filter(|s| pred(&s.event))
            .count()
    }

    /// Eerste event waarvoor `pred` true is.
    pub(crate) fn find<F: Fn(&UiEvent) -> bool>(&self, pred: F) -> Option<UiEvent> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .find(|s| pred(&s.event))
            .map(|s| s.event.clone())
    }

    /// Verifieer dat een gegeven `intent_id` zijn chain netjes afsluit:
    /// één `IntentEmitted` + exact één van `CommandSent` / `CommandBlocked` /
    /// `CommandSendFailed`, geen duplicaten.
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
