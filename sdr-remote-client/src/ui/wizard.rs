// SPDX-License-Identifier: GPL-2.0-or-later

//! PATCH-4: first-run connection wizard.
//!
//! Drives the user through Discover -> Password -> (2FA) -> Verifying ->
//! Success in 4 visible steps. Reactive: subscribes to the same
//! `RadioState.connect_status` that PATCH-1 produces and renders the
//! corresponding step. The wizard never invents new connect-state - it
//! only navigates around it, so a fault that PATCH-1 already classifies
//! (`WrongPassword`, `AwaitingTotp`, `TciUnreachable`, ...) lands the
//! user on the correct previous step with the matching i18n text.

use egui::{Color32, RichText};

use sdr_remote_logic::commands::Command;
use sdr_remote_logic::i18n::{connect_error_text, Lang, Platform};
use sdr_remote_logic::state::{ConnectError, ConnectStatus, RadioState};
use tokio::sync::{mpsc, watch};

/// Where the wizard is in its flow. Mirrors `ConnectStatus` plus
/// pre-Connect data-entry steps that PATCH-1 doesn't model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WizardStep {
    DiscoverServer,
    EnterPassword,
    Verifying,
    AwaitingTotp,
    Verifying2fa,
    Success,
}

/// Result of a single `render()` call - tells the host UI what to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WizardOutcome {
    /// Stay in wizard mode; render again next frame.
    Continue,
    /// User pressed "Skip wizard" - switch to manual form without
    /// touching the config (per brief §4 Stap 4: no `successful_connects`
    /// bump on skip - otherwise the wizard never re-appears).
    SkipToManual,
    /// Server returned `ConnectStatus::Connected` and the user pressed
    /// Done. Config has been marked as a successful connect; subsequent
    /// app starts will boot straight into the regular UI.
    Finished,
}

/// All wizard-local state. Lives in `ServerApp` while `Mode::Wizard`.
pub(crate) struct WizardState {
    pub(crate) step: WizardStep,
    pub(crate) server_input: String,
    pub(crate) password_input: String,
    pub(crate) password_visible: bool,
    pub(crate) totp_input: String,
    /// Sticky copy of the last connect error so the rendered pane keeps
    /// showing the same headline even when `connect_status` already
    /// transitioned back to `Disconnected` after we sent a new Connect.
    pub(crate) last_error: Option<ConnectError>,
    /// Tracks the previous `connect_status` so transitions trigger
    /// step changes only on the rising edge.
    pub(crate) prev_status: ConnectStatus,
}

impl WizardState {
    pub(crate) fn new(server: String, password: String) -> Self {
        Self {
            step: WizardStep::DiscoverServer,
            server_input: server,
            password_input: password,
            password_visible: false,
            totp_input: String::new(),
            last_error: None,
            prev_status: ConnectStatus::Disconnected,
        }
    }
}

/// One frame of wizard rendering. Reads the current `RadioState`,
/// transitions the wizard step on state changes, renders the active
/// pane, and dispatches `Command::Connect` / `SendTotpCode` when the
/// user advances.
pub(crate) fn render_wizard(
    ui: &mut egui::Ui,
    state: &mut WizardState,
    cmd_tx: &mpsc::UnboundedSender<Command>,
    state_rx: &watch::Receiver<RadioState>,
    mdns: Option<&crate::mdns::BrowseHandle>,
    lang: Lang,
) -> WizardOutcome {
    // ── React to logic-state transitions ────────────────────────────────
    let cur = state_rx.borrow().connect_status.clone();
    let transitioned = cur != state.prev_status;
    if transitioned {
        match &cur {
            ConnectStatus::AwaitingTotp => {
                state.step = WizardStep::AwaitingTotp;
                state.last_error = None;
            }
            ConnectStatus::Connected => {
                state.step = WizardStep::Success;
                state.last_error = None;
            }
            ConnectStatus::Failed(err) => {
                state.last_error = Some(err.clone());
                // Land the user on the step that can actually fix the
                // problem: wrong-totp -> AwaitingTotp; everything else
                // -> Password (covers wrong-password) or DiscoverServer
                // (covers DNS/unreachable). Operator can also press Back.
                state.step = match err {
                    ConnectError::WrongTotp => WizardStep::AwaitingTotp,
                    ConnectError::WrongPassword => WizardStep::EnterPassword,
                    ConnectError::DnsResolutionFailed { .. }
                    | ConnectError::NoUdpResponse { .. }
                    | ConnectError::MalformedResponse { .. }
                    | ConnectError::ProtocolVersionMismatch { .. } => WizardStep::DiscoverServer,
                    ConnectError::TciUnreachable { .. } | ConnectError::Other { .. } => {
                        // Still useful to be in the verifying pane so
                        // the i18n action-hint can be shown alongside.
                        WizardStep::EnterPassword
                    }
                };
            }
            _ => {}
        }
        state.prev_status = cur.clone();
    }

    // ── Progress bar + step header ──────────────────────────────────────
    let (step_idx, step_total) = match state.step {
        WizardStep::DiscoverServer => (1, 4),
        WizardStep::EnterPassword | WizardStep::Verifying => (2, 4),
        WizardStep::AwaitingTotp | WizardStep::Verifying2fa => (3, 4),
        WizardStep::Success => (4, 4),
    };
    let step_label = match state.step {
        WizardStep::DiscoverServer => lang.pick(
            "Find the server",
            "Vind de server",
            "Server finden",
            "Trouver le serveur",
        ),
        WizardStep::EnterPassword | WizardStep::Verifying => lang.pick(
            "Enter password",
            "Wachtwoord invoeren",
            "Passwort eingeben",
            "Saisir le mot de passe",
        ),
        WizardStep::AwaitingTotp | WizardStep::Verifying2fa => {
            lang.pick("2FA code", "2FA-code", "2FA-Code", "Code 2FA")
        }
        WizardStep::Success => lang.pick("Connected", "Verbonden", "Verbunden", "Connecté"),
    };
    let mut skip_clicked = false;
    ui.horizontal(|ui| {
        // "Step 2 of 4" was English on every screen, including the Dutch one -
        // the step NAME was translated and the sentence around it was not.
        ui.heading(lang.pick(
            &format!("Step {step_idx} of {step_total}: {step_label}"),
            &format!("Stap {step_idx} van {step_total}: {step_label}"),
            &format!("Schritt {step_idx} von {step_total}: {step_label}"),
            &format!("Étape {step_idx} sur {step_total} : {step_label}"),
        ));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .small_button(lang.pick(
                    "Skip wizard",
                    "Wizard overslaan",
                    "Assistent überspringen",
                    "Passer l'assistant",
                ))
                .on_hover_text(lang.pick(
                    "Jump straight to the regular connect screen. Config is not updated.",
                    "Direct naar het standaard connect-scherm. Je config wordt niet bijgewerkt.",
                    "Direkt zum normalen Verbindungsbildschirm. Die Konfiguration wird nicht geändert.",
                    "Aller directement à l'écran de connexion habituel. La configuration n'est pas modifiée.",
                ))
                .clicked()
            {
                skip_clicked = true;
            }
        });
    });
    if skip_clicked {
        return WizardOutcome::SkipToManual;
    }
    ui.add(egui::ProgressBar::new(step_idx as f32 / step_total as f32).desired_width(360.0));
    ui.separator();

    // ── Active pane ─────────────────────────────────────────────────────
    let mut outcome = WizardOutcome::Continue;
    match state.step.clone() {
        WizardStep::DiscoverServer => {
            outcome = render_discover(ui, state, mdns, lang);
        }
        WizardStep::EnterPassword => {
            outcome = render_password(ui, state, cmd_tx, lang);
        }
        WizardStep::Verifying => {
            render_verifying(ui, lang, false);
        }
        WizardStep::AwaitingTotp => {
            outcome = render_awaiting_totp(ui, state, cmd_tx, lang);
        }
        WizardStep::Verifying2fa => {
            render_verifying(ui, lang, true);
        }
        WizardStep::Success => {
            outcome = render_success(ui, lang);
        }
    }

    // ── Sticky error footer (PATCH-1 i18n) ──────────────────────────────
    if let Some(ref err) = state.last_error {
        let (headline, action) = connect_error_text(err, lang, Platform::Desktop);
        ui.separator();
        ui.label(
            RichText::new(&headline)
                .size(16.0)
                .strong()
                .color(Color32::from_rgb(220, 60, 60)),
        );
        if let Some(a) = action {
            ui.label(RichText::new(a).size(14.0));
        }
    }

    outcome
}

fn render_discover(
    ui: &mut egui::Ui,
    state: &mut WizardState,
    mdns: Option<&crate::mdns::BrowseHandle>,
    lang: Lang,
) -> WizardOutcome {
    ui.label(lang.pick(
        "Pick a server from the list or enter the address manually.",
        "Kies een server uit de lijst of voer het adres handmatig in.",
        "Wählen Sie einen Server aus der Liste oder geben Sie die Adresse von Hand ein.",
        "Choisissez un serveur dans la liste ou saisissez l'adresse manuellement.",
    ));
    if let Some(handle) = mdns {
        let servers = handle.snapshot();
        if servers.is_empty() {
            ui.label(
                RichText::new(lang.pick(
                    "Scanning local network...",
                    "Bezig met scannen van het lokale netwerk...",
                    "Lokales Netzwerk wird durchsucht...",
                    "Analyse du réseau local...",
                ))
                .size(11.0)
                .color(Color32::from_rgb(150, 150, 150)),
            );
        } else {
            ui.label(lang.pick(
                "Found on this network:",
                "Gevonden in het netwerk:",
                "In diesem Netzwerk gefunden:",
                "Trouvés sur ce réseau :",
            ));
            for srv in &servers {
                if ui.selectable_label(false, srv.display_label()).clicked() {
                    state.server_input = srv.addr_port.clone();
                }
            }
        }
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(lang.pick("Address:", "Adres:", "Adresse:", "Adresse :"));
        ui.add(
            egui::TextEdit::singleline(&mut state.server_input)
                .hint_text("192.168.1.79:4580")
                .desired_width(180.0),
        );
    });
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let next_label = lang.pick("Next", "Volgende", "Weiter", "Suivant");
        let enabled = !state.server_input.trim().is_empty();
        if ui.add_enabled(enabled, egui::Button::new(next_label)).clicked() {
            state.step = WizardStep::EnterPassword;
        }
    });
    WizardOutcome::Continue
}

fn render_password(
    ui: &mut egui::Ui,
    state: &mut WizardState,
    cmd_tx: &mpsc::UnboundedSender<Command>,
    lang: Lang,
) -> WizardOutcome {
    ui.label(lang.pick(
        "Enter the server password. Ask the operator of the server PC for it.",
        "Vul het wachtwoord van de server in. Vraag dit aan de operator van de server-PC.",
        "Geben Sie das Serverpasswort ein. Fragen Sie danach beim Betreiber des Server-PCs.",
        "Saisissez le mot de passe du serveur. Demandez-le à l'opérateur du PC serveur.",
    ));
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(lang.pick("Password:", "Wachtwoord:", "Passwort:", "Mot de passe :"));
        let mut edit = egui::TextEdit::singleline(&mut state.password_input).desired_width(180.0);
        if !state.password_visible {
            edit = edit.password(true);
        }
        ui.add(edit);
        ui.checkbox(
            &mut state.password_visible,
            lang.pick("Show", "Toon", "Anzeigen", "Afficher"),
        );
    });
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let back_label = lang.pick("Back", "Vorige", "Zurück", "Retour");
        let next_label = lang.pick("Connect", "Verbind", "Verbinden", "Se connecter");
        if ui.button(back_label).clicked() {
            state.step = WizardStep::DiscoverServer;
            state.last_error = None;
        }
        let pw_ok = !state.password_input.trim().is_empty();
        if ui.add_enabled(pw_ok, egui::Button::new(next_label)).clicked() {
            state.last_error = None;
            let _ = cmd_tx.send(Command::Connect(
                state.server_input.trim().to_string(),
                Some(state.password_input.clone()),
            ));
            state.step = WizardStep::Verifying;
        }
    });
    WizardOutcome::Continue
}

fn render_verifying(ui: &mut egui::Ui, lang: Lang, is_totp: bool) {
    ui.add(egui::Spinner::new());
    let label = if is_totp {
        lang.pick(
            "Verifying 2FA code...",
            "Bezig met verifiëren van 2FA-code...",
            "2FA-Code wird geprüft...",
            "Vérification du code 2FA...",
        )
    } else {
        lang.pick(
            "Connecting...",
            "Bezig met verbinden...",
            "Verbindung wird hergestellt...",
            "Connexion en cours...",
        )
    };
    ui.label(RichText::new(label).size(14.0));
}

fn render_awaiting_totp(
    ui: &mut egui::Ui,
    state: &mut WizardState,
    cmd_tx: &mpsc::UnboundedSender<Command>,
    lang: Lang,
) -> WizardOutcome {
    ui.label(lang.pick(
        "Open your authenticator app and enter the 6-digit code.",
        "Open je authenticator-app en vul de 6-cijferige code in.",
        "Öffnen Sie Ihre Authenticator-App und geben Sie den 6-stelligen Code ein.",
        "Ouvrez votre application d'authentification et saisissez le code à 6 chiffres.",
    ));
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(lang.pick("Code:", "Code:", "Code:", "Code :"));
        ui.add(
            egui::TextEdit::singleline(&mut state.totp_input)
                .desired_width(80.0)
                .hint_text("000000"),
        );
    });
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let back_label = lang.pick("Back", "Vorige", "Zurück", "Retour");
        let verify_label = lang.pick("Verify", "Verifieer", "Prüfen", "Vérifier");
        if ui.button(back_label).clicked() {
            // Cancel current session: a fresh attempt would otherwise
            // collide with the server's PendingTotp state.
            let _ = cmd_tx.send(Command::Disconnect);
            state.step = WizardStep::EnterPassword;
            state.last_error = None;
        }
        let code_ok = state.totp_input.len() == 6;
        if ui.add_enabled(code_ok, egui::Button::new(verify_label)).clicked() {
            state.last_error = None;
            let _ = cmd_tx.send(Command::SendTotpCode(state.totp_input.clone()));
            state.totp_input.clear();
            state.step = WizardStep::Verifying2fa;
        }
    });
    WizardOutcome::Continue
}

fn render_success(ui: &mut egui::Ui, lang: Lang) -> WizardOutcome {
    ui.colored_label(
        Color32::from_rgb(50, 200, 50),
        RichText::new(lang.pick(
            "Connected!",
            "Verbonden!",
            "Verbunden!",
            "Connecté !",
        ))
        .size(20.0)
        .strong(),
    );
    ui.add_space(4.0);
    ui.label(lang.pick(
        "Next time you start the client the wizard is skipped automatically.",
        "De volgende keer dat je de client start hoef je de wizard niet meer te doorlopen.",
        "Beim nächsten Start des Clients wird der Assistent automatisch übersprungen.",
        "Au prochain démarrage du client, l'assistant sera automatiquement ignoré.",
    ));
    ui.add_space(8.0);
    if ui
        .button(lang.pick("Done", "Klaar", "Fertig", "Terminé"))
        .clicked()
    {
        return WizardOutcome::Finished;
    }
    WizardOutcome::Continue
}
