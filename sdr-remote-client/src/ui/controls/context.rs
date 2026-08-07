// SPDX-License-Identifier: GPL-2.0-or-later

//! `ControlContext` bundles everything a control helper needs: flags,
//! state, command-sender, event sink. This way every helper has a single
//! parameter and enabled/observability/state access is uniform.
//!
//! Deliberately no `&mut ThetisLinkApp` - that would expose the whole app-state
//! and make tests impossible without full app construction.
//!
//! `cmd_tx` is private: helpers outside this module can
//! NOT call `ctx.cmd_tx.send(...)` directly. The only route is
//! `dispatch()`, which enforces that every command has a preceding
//! `IntentEmitted` and respects the connected-guard.

use sdr_remote_logic::commands::Command;
use tokio::sync::mpsc;

use super::events::{CommandBlockReason, UiEvent, UiEventSink, UiIntent};
use super::{RxChannel, RxChannelState, SharedUiState, UiDensity, UiSurface};

pub(crate) struct ControlContext<'a> {
    pub(crate) connected: bool,
    pub(crate) density: UiDensity,
    pub(crate) surface: UiSurface,
    pub(crate) channel: RxChannel,
    cmd_tx: &'a mpsc::UnboundedSender<Command>,
    pub(crate) rx_state: &'a mut RxChannelState,
    pub(crate) shared: &'a mut SharedUiState,
    pub(crate) events: &'a dyn UiEventSink,
}

impl<'a> ControlContext<'a> {
    /// Constructor: `cmd_tx` is made private here, helpers cannot reach it
    /// directly outside `dispatch()`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        connected: bool,
        density: UiDensity,
        surface: UiSurface,
        channel: RxChannel,
        cmd_tx: &'a mpsc::UnboundedSender<Command>,
        rx_state: &'a mut RxChannelState,
        shared: &'a mut SharedUiState,
        events: &'a dyn UiEventSink,
    ) -> Self {
        Self {
            connected,
            density,
            surface,
            channel,
            cmd_tx,
            rx_state,
            shared,
            events,
        }
    }

    /// Emit a `UiEvent` via the configured sink.
    pub(crate) fn emit(&self, event: UiEvent) {
        self.events.emit(event);
    }

    /// Canonical route to send a command from a control helper.
    ///
    /// Guarantees:
    /// - every `cmd_tx.send` is preceded by an `IntentEmitted` with
    ///   its associated `intent_id`;
    /// - when `connected == false` the command is NOT sent; instead
    ///   it emits a `CommandBlocked { reason: Disconnected }`;
    /// - on send failure (channel closed) it emits a `CommandSendFailed` instead
    ///   of a false-positive `CommandSent`.
    ///
    /// Returns `true` only if the command was actually delivered.
    ///
    /// `#[must_use]`: callers MUST check the return value before they
    /// mutate local UI-state. If they don't, state-drift can arise
    /// between client and server - exactly the bug class from
    /// PATCH-client-band-switch-guard finding #3 that this refactor was meant
    /// to eliminate. The compiler enforces the contract.
    #[must_use]
    pub(crate) fn dispatch(&self, intent: UiIntent, command: Command) -> bool {
        let kind = intent.kind();
        let intent_id = self.events.record_intent(&intent, self.connected);
        if !self.connected {
            self.events.emit(UiEvent::CommandBlocked {
                intent_kind: kind,
                reason: CommandBlockReason::Disconnected,
                intent_id,
            });
            return false;
        }
        match self.cmd_tx.send(command) {
            Ok(()) => {
                self.events.emit(UiEvent::CommandSent {
                    intent_kind: kind,
                    connected: true,
                    intent_id,
                });
                true
            }
            Err(_) => {
                self.events.emit(UiEvent::CommandSendFailed {
                    intent_kind: kind,
                    intent_id,
                });
                false
            }
        }
    }
}
