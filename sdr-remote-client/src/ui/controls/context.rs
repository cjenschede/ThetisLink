// SPDX-License-Identifier: GPL-2.0-or-later

//! `ControlContext` bundelt alles wat een control-helper nodig heeft: flags,
//! state, command-sender, event sink. Zo heeft elke helper één parameter en
//! is enabled/observability/state-toegang uniform.
//!
//! Bewust géén `&mut ThetisLinkApp` — dat zou de hele app-state exposen en
//! tests onmogelijk maken zonder volledige app-constructie.
//!
//! `cmd_tx` is privé: helpers buiten deze module kunnen
//! NIET direct `ctx.cmd_tx.send(...)` aanroepen. De enige route is
//! `dispatch()`, wat afdwingt dat elke command een voorafgaande `IntentEmitted`
//! heeft en de connected-guard respecteert.

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
    /// Constructeur: `cmd_tx` wordt hier privé gezet, helpers kunnen hem niet
    /// direct bereiken buiten `dispatch()`.
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

    /// Emit een `UiEvent` via de geconfigureerde sink.
    pub(crate) fn emit(&self, event: UiEvent) {
        self.events.emit(event);
    }

    /// Canonieke route om een command te versturen vanuit een control-helper.
    ///
    /// Garanties:
    /// - elke `cmd_tx.send` wordt voorafgegaan door een `IntentEmitted` met
    ///   bijhorende `intent_id`;
    /// - bij `connected == false` wordt het command NIET verstuurd; in plaats
    ///   daarvan emit een `CommandBlocked { reason: Disconnected }`;
    /// - bij send-falen (kanaal gesloten) emit een `CommandSendFailed` i.p.v.
    ///   een vals-positieve `CommandSent`.
    ///
    /// Retourneert `true` alleen als het command daadwerkelijk is afgeleverd.
    ///
    /// `#[must_use]`: callers MOETEN de return-waarde checken voordat ze
    /// lokale UI-state muteren. Als ze dat niet doen, kan state-drift ontstaan
    /// tussen client en server — exact de bug-klasse uit
    /// PATCH-client-band-switch-guard finding #3 die deze refactor moest
    /// wegnemen. Compiler dwingt het contract af.
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
