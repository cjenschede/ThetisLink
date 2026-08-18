// SPDX-License-Identifier: GPL-2.0-or-later
//
//! The chat as state, without a pixel in sight.
//!
//! Everything the three front ends agree about lives here: the worker thread and
//! its two channels, what has arrived, who we are to the service, and when to
//! ask again. The egui window ([`crate::ChatPanel`]) draws this; the Android
//! bridge hands the same fields to Compose. Neither owns a second copy of the
//! rules, which is the whole reason this file exists - a conversation that
//! behaves differently on a phone than on a desktop is two chats, and the
//! second one is the one nobody tests.
//!
//! Nothing here blocks. The worker does the network on its own thread and this
//! only drains what it sends back, so a chat service that is down or slow
//! cannot get between an operator and their PTT.

use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use crate::worker::{ChatAnswer, ChatCommand, ChatEvent, ChatMessage, OfflineReason};

/// How long an answer poll waits. One per report and rare; asking with the
/// conversation's own rhythm would be noise.
const ANSWER_INTERVAL: Duration = Duration::from_secs(30);

/// The version of the consent text THIS BUILD carries.
///
/// It lives beside the text itself (`locales/app.yml`) because that is the
/// thing it describes: raise it when the text changes materially, and a
/// service on a newer text will refuse a consent from a build still showing
/// the old one. Kept in step by hand with the service's own constant - two
/// numbers that must match, and the mismatch is exactly what the refusal is
/// for.
pub const CONSENT_TEXT_VERSION: i64 = 3;

/// How long a message stays its author's to correct, as this side sees it.
///
/// The service allows fifteen minutes and is the judge; this stops offering
/// half a minute earlier, because clocks differ a little between machines and a
/// button that works right up to a boundary it cannot see would sometimes open
/// a field the service is about to refuse.
pub const EDIT_WINDOW_SHOWN_SECS: i64 = 15 * 60 - 30;

/// The window's own memory of the conversation. The service decides what is
/// kept; this only refuses to grow without end.
const MAX_HELD: usize = 500;

/// The chat, as state.
pub struct ChatModel {
    tx: Option<Sender<ChatCommand>>,
    rx: Option<Receiver<ChatEvent>>,
    last_poll: Option<Instant>,
    last_answer_poll: Option<Instant>,

    /// `None` until the service has said. Neither screen is shown before then,
    /// because guessing wrong means showing somebody a consent form they
    /// already filled in.
    pub consented: Option<bool>,
    /// The name others see, as the service knows it.
    pub display_name: String,
    /// Which version of the consent text this build shows, from the last state
    /// event. It goes back with the consent itself, so the service can see that
    /// agreement was given to the text that was actually on screen.
    pub consent_text_version: i64,
    pub messages: Vec<ChatMessage>,
    pub last_id: i64,
    pub unread: usize,
    pub offline: Option<OfflineReason>,
    pub error: Option<String>,
    /// How many problem reports this station may still send today; -1 when the
    /// service has not said. Known before a form is opened, because being told
    /// at the send button is being told after the work.
    pub reports_left: i64,
    /// What the administrator has answered on this station's problem reports.
    pub answers: Vec<ChatAnswer>,
    /// A report reached the postbox and nobody has been told yet.
    ///
    /// Taken rather than read (see [`Self::take_diagnosis_sent`]), because the
    /// only thing that acts on it is the form closing itself, and that must
    /// happen once. This is the flag whose absence let a form stay open after
    /// a successful send: the sender saw nothing happen, pressed send again,
    /// and again, and reached the day's limit that way (2026-08-12).
    diagnosis_sent: bool,
}

impl Default for ChatModel {
    fn default() -> Self {
        Self {
            tx: None,
            rx: None,
            last_poll: None,
            last_answer_poll: None,
            consented: None,
            display_name: String::new(),
            // Until the service says otherwise this is what the build shows.
            consent_text_version: 1,
            messages: Vec::new(),
            last_id: 0,
            unread: 0,
            // Until a relay says otherwise, there is nothing to talk to. Saying
            // which of the three (§8) beats one word covering all of them.
            offline: Some(OfflineReason::NoRelay),
            error: None,
            reports_left: -1,
            answers: Vec::new(),
            diagnosis_sent: false,
        }
    }
}

impl ChatModel {
    /// Start the worker once, keep it told where to reach the chat, and take in
    /// whatever it has sent back.
    ///
    /// Called every frame (desktop) or every UI tick (Android); everything in it
    /// is cheap and idempotent, because the alternative is a lifecycle spread
    /// over half a dozen call sites.
    ///
    /// `open` is the host's own flag for whether its chat screen is showing:
    /// while it is, the conversation is polled briskly; while it is not, once
    /// every half minute, which is what feeds the unread count on whatever
    /// button opens it (design §1.7). A counter that only counts while you are
    /// already watching counts nothing.
    pub fn tick(&mut self, relay_url: &str, ticket: Option<&str>, open: bool) {
        if self.tx.is_none() {
            let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
            let (evt_tx, evt_rx) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .name("chat".to_string())
                .spawn(move || crate::worker::run(cmd_rx, evt_tx))
                .ok();
            self.tx = Some(cmd_tx);
            self.rx = Some(evt_rx);
        }

        // Endpoint and ticket are sent every tick rather than tracked for
        // changes: they are two tiny messages on a channel nobody else uses, and
        // a missed change would leave the window mysteriously offline.
        if let Some(tx) = &self.tx {
            let _ = tx.send(ChatCommand::Endpoint(crate::worker::endpoint_for_relay(relay_url)));
            let _ = tx.send(ChatCommand::Ticket(ticket.map(str::to_string)));
        }

        self.drain();

        // Members only: before consent there is no conversation to have news
        // from, and the service would refuse the ask anyway.
        if !open && self.consented == Some(true) && self.due(crate::worker::POLL_INTERVAL_CLOSED) {
            self.last_poll = Some(Instant::now());
            self.send_cmd(ChatCommand::Poll { since: self.last_id });
        }
        if open {
            let answers_due = self
                .last_answer_poll
                .map(|t| t.elapsed() >= ANSWER_INTERVAL)
                .unwrap_or(true);
            if answers_due {
                self.last_answer_poll = Some(Instant::now());
                self.send_cmd(ChatCommand::Answers);
            }
            if self.due(crate::worker::POLL_INTERVAL) {
                self.last_poll = Some(Instant::now());
                if self.consented == Some(true) {
                    self.send_cmd(ChatCommand::Poll { since: self.last_id });
                } else {
                    self.send_cmd(ChatCommand::Refresh);
                }
            }
        }
    }

    fn due(&self, every: Duration) -> bool {
        self.last_poll.map(|t| t.elapsed() >= every).unwrap_or(true)
    }

    /// Ask again at the next tick rather than waiting out the interval - used
    /// after sending, so your own words appear without a pause.
    pub fn poll_soon(&mut self) {
        self.last_poll = None;
    }

    fn send_cmd(&self, cmd: ChatCommand) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(cmd);
        }
    }

    /// Take in everything the worker has sent since the last call.
    ///
    /// `try_recv` in a loop: the UI thread may never block on this.
    pub fn drain(&mut self) {
        let Some(rx) = &self.rx else { return };
        let mut events = Vec::new();
        while let Ok(evt) = rx.try_recv() {
            events.push(evt);
        }
        for evt in events {
            self.apply(evt);
        }
    }

    /// One event, applied. Split out so the rules can be tested without a
    /// worker thread, a network, or a service.
    pub fn apply(&mut self, evt: ChatEvent) {
        // Anything the service answers is proof the last refusal is over.
        //
        // Here rather than in the arms, and that is the whole point: the
        // message used to be cleared only when the operator did something, so
        // a lifted ban left "you have been removed by the administrator" on
        // screen while the chat was already working - and that is the one
        // refusal after which nobody tries again to find out. Putting the
        // clearing in the `State` arm did not fix it either: a member polls
        // for MESSAGES, and only a non-member asks for state, so the arm that
        // was mended is the arm that never runs for the person affected.
        // One place, no list of events to keep in step (2026-08-16).
        if !matches!(evt, ChatEvent::Failed(_) | ChatEvent::Offline(_)) {
            self.error = None;
        }
        match evt {
            ChatEvent::State { consented, display_name, consent_text_version, reports_left } => {
                self.offline = None;
                self.reports_left = reports_left;
                self.consented = Some(consented);
                if let Some(n) = display_name {
                    self.display_name = n;
                }
                self.consent_text_version = consent_text_version;
                if !consented {
                    // Left the chat: drop what was on screen, or the window
                    // would keep showing a conversation you just left.
                    self.messages.clear();
                    self.last_id = 0;
                    self.unread = 0;
                }
            }
            ChatEvent::Messages { new, edited } => {
                self.offline = None;
                for m in new {
                    self.last_id = self.last_id.max(m.id);
                    // Your own messages are not unread news to you.
                    if !self.is_mine(&m) {
                        self.unread = self.unread.saturating_add(1);
                    }
                    self.messages.push(m);
                }
                // Corrections land in place: same id, final text, marker on.
                // Not unread - the author fixed a word, nobody said anything
                // new - and not appended, or the fix would arrive as a second
                // copy below the original.
                for e in edited {
                    if let Some(held) = self.messages.iter_mut().find(|m| m.id == e.id) {
                        *held = e;
                    }
                }
                if self.messages.len() > MAX_HELD {
                    let drop = self.messages.len() - MAX_HELD;
                    self.messages.drain(0..drop);
                }
            }
            ChatEvent::Answers(list) => {
                self.offline = None;
                self.answers = list;
            }
            ChatEvent::DiagnosisSent => {
                self.offline = None;
                self.diagnosis_sent = true;
                // One less for today, without waiting for the next state poll:
                // the allowance shown on a form that is closing should already
                // be the allowance that remains.
                if self.reports_left > 0 {
                    self.reports_left -= 1;
                }
            }
            ChatEvent::Failed(why) => {
                self.offline = None;
                self.error = Some(why);
            }
            ChatEvent::Offline(why) => {
                // Not an error: a chat that is not there is a state, and the
                // three reasons need three different things from the user.
                self.offline = Some(why);
            }
        }
    }

    /// Written by us, as far as this side can tell.
    ///
    /// By name, which is what the wire carries. The service decides the same
    /// thing by station id and is the judge; this only avoids offering what
    /// would be refused.
    pub fn is_mine(&self, m: &ChatMessage) -> bool {
        !self.display_name.is_empty() && m.name.as_deref() == Some(self.display_name.as_str())
    }

    /// May this message still be corrected by us? See
    /// [`EDIT_WINDOW_SHOWN_SECS`] for why this closes early.
    pub fn can_edit(&self, m: &ChatMessage, now: i64) -> bool {
        self.is_mine(m) && now - m.at < EDIT_WINDOW_SHOWN_SECS
    }

    /// Everything read; the badge goes out.
    pub fn mark_read(&mut self) {
        self.unread = 0;
    }

    /// Has a report arrived at the postbox since this was last asked?
    ///
    /// Taken, not read: the form closes on it, and a flag that stays set would
    /// close the next form the moment it opens.
    pub fn take_diagnosis_sent(&mut self) -> bool {
        std::mem::take(&mut self.diagnosis_sent)
    }

    // ---- the things a user does -----------------------------------------

    pub fn consent(&mut self, display_name: &str) {
        self.error = None;
        self.send_cmd(ChatCommand::Consent {
            display_name: display_name.trim().to_string(),
            // The version THIS BUILD shows, not the one the service just said
            // it was on. Echoing the service back meant the service compared
            // its own constant with its own answer and was always satisfied -
            // so the refusal that exists to stop a consent being recorded
            // against a text the client never displayed could not fire for any
            // client that had ever fetched state, which is every client. The
            // record then said people had agreed to a text they had not seen,
            // which is worse than keeping no version at all (2026-08-16).
            text_version: CONSENT_TEXT_VERSION,
        });
        self.poll_soon();
    }

    pub fn send(&mut self, body: &str, reply_to: Option<i64>) {
        if body.trim().is_empty() {
            return;
        }
        self.error = None;
        self.send_cmd(ChatCommand::Send {
            body: body.trim().to_string(),
            reply_to,
        });
        self.poll_soon();
    }

    pub fn edit(&mut self, id: i64, body: &str) {
        if body.trim().is_empty() {
            return;
        }
        self.error = None;
        self.send_cmd(ChatCommand::Edit {
            id,
            body: body.trim().to_string(),
        });
        self.poll_soon();
    }

    pub fn leave(&mut self, delete_messages: bool) {
        self.error = None;
        self.send_cmd(ChatCommand::Leave { delete_messages });
        self.poll_soon();
    }

    /// One problem report, already cleaned and already seen by the user. The
    /// redaction happens before this: what leaves the machine has to be what was
    /// shown on screen, or the preview is theatre (design 1.3).
    pub fn send_diagnosis(&mut self, report: &str) {
        self.error = None;
        self.send_cmd(ChatCommand::SendDiagnosis {
            report: report.to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: i64, name: &str, body: &str, at: i64) -> ChatMessage {
        ChatMessage {
            id,
            at,
            name: Some(name.to_string()),
            body: body.to_string(),
            reply_name: None,
            reply_text: None,
            edited: false,
        }
    }

    fn joined() -> ChatModel {
        let mut m = ChatModel::default();
        m.apply(ChatEvent::State {
            consented: true,
            display_name: Some("PA0ABC".into()),
            consent_text_version: 2,
            reports_left: 100,
        });
        m
    }

    #[test]
    fn arriving_messages_count_as_unread_except_your_own() {
        let mut m = joined();
        m.apply(ChatEvent::Messages {
            new: vec![msg(1, "PD9XYZ", "hoi", 100), msg(2, "PA0ABC", "hallo", 101)],
            edited: vec![],
        });
        assert_eq!(m.unread, 1, "only the other station's message is news");
        assert_eq!(m.last_id, 2);
        m.mark_read();
        assert_eq!(m.unread, 0);
    }

    /// A correction replaces its original in place. Appending it would show the
    /// fix as a second copy below the typo, which is worse than the typo.
    #[test]
    fn a_correction_lands_in_place_and_is_not_news() {
        let mut m = joined();
        m.apply(ChatEvent::Messages { new: vec![msg(1, "PD9XYZ", "hoi", 100)], edited: vec![] });
        m.mark_read();
        let mut fixed = msg(1, "PD9XYZ", "hoi allemaal", 100);
        fixed.edited = true;
        m.apply(ChatEvent::Messages { new: vec![], edited: vec![fixed] });
        assert_eq!(m.messages.len(), 1);
        assert_eq!(m.messages[0].body, "hoi allemaal");
        assert!(m.messages[0].edited);
        assert_eq!(m.unread, 0, "a fixed word is not new news");
    }

    /// Leaving takes the conversation off the screen with it: what is shown
    /// after a leave would be a chat you are no longer in.
    #[test]
    fn leaving_clears_what_was_on_screen() {
        let mut m = joined();
        m.apply(ChatEvent::Messages { new: vec![msg(1, "PD9XYZ", "hoi", 100)], edited: vec![] });
        m.apply(ChatEvent::State {
            consented: false,
            display_name: None,
            consent_text_version: 2,
            reports_left: 100,
        });
        assert!(m.messages.is_empty());
        assert_eq!(m.last_id, 0);
        assert_eq!(m.unread, 0);
    }

    #[test]
    fn only_your_own_recent_message_offers_a_correction() {
        let m = joined();
        let mine = msg(1, "PA0ABC", "tekst", 1_000);
        let theirs = msg(2, "PD9XYZ", "tekst", 1_000);
        assert!(m.can_edit(&mine, 1_000 + 60));
        assert!(!m.can_edit(&theirs, 1_000 + 60), "not yours to fix");
        assert!(
            !m.can_edit(&mine, 1_000 + EDIT_WINDOW_SHOWN_SECS),
            "the window closes before the service's own limit"
        );
    }

    /// A report that arrived says so exactly once - the form closes on it, and
    /// a flag that stayed set would shut the next form as it opened. Its
    /// absence is what let a sent report look like nothing happening, which
    /// cost fifteen duplicate reports before anybody could report the fault.
    #[test]
    fn a_sent_report_is_announced_once() {
        let mut m = joined();
        assert!(!m.take_diagnosis_sent(), "nothing sent yet");
        m.apply(ChatEvent::DiagnosisSent);
        assert!(m.take_diagnosis_sent(), "the form is told");
        assert!(!m.take_diagnosis_sent(), "and only once");
    }

    /// The allowance drops as soon as one is accepted, so a form that stays
    /// open for another report shows what is really left.
    #[test]
    fn sending_uses_up_one_of_todays_reports() {
        let mut m = joined();
        m.apply(ChatEvent::State {
            consented: true,
            display_name: Some("PA0ABC".into()),
            consent_text_version: 2,
            reports_left: 2,
        });
        m.apply(ChatEvent::DiagnosisSent);
        assert_eq!(m.reports_left, 1);
        m.apply(ChatEvent::DiagnosisSent);
        assert_eq!(m.reports_left, 0);
        // And it does not go negative when the service has not said.
        let mut unknown = ChatModel::default();
        unknown.apply(ChatEvent::DiagnosisSent);
        assert_eq!(unknown.reports_left, -1, "unknown stays unknown");
    }

    /// A message from somebody who left has no name at all; it must not read as
    /// ours just because we have no name either.
    #[test]
    fn an_anonymous_message_is_nobodys() {
        let mut m = ChatModel::default();
        let left = ChatMessage {
            id: 1,
            at: 100,
            name: None,
            body: "van iemand die weg is".into(),
            reply_name: None,
            reply_text: None,
            edited: false,
        };
        assert!(!m.is_mine(&left));
        m.display_name = "PA0ABC".into();
        assert!(!m.is_mine(&left));
    }

    /// The store holds at most what it says it holds, whatever the service
    /// sends.
    #[test]
    fn the_held_conversation_stays_bounded() {
        let mut m = joined();
        let new: Vec<ChatMessage> =
            (1..=MAX_HELD as i64 + 20).map(|i| msg(i, "PD9XYZ", "x", 100)).collect();
        m.apply(ChatEvent::Messages { new, edited: vec![] });
        assert_eq!(m.messages.len(), MAX_HELD);
        assert_eq!(m.messages[0].id, 21, "the oldest go first");
    }

    /// A refusal that has been lifted must leave the screen on its own.
    ///
    /// It did not: the message was cleared only when the operator acted, so a
    /// ban that was taken off left "you have been removed by the
    /// administrator" standing while the chat was working again. The poll
    /// coming back IS the news, and it has to be able to deliver it.
    #[test]
    fn a_refusal_clears_itself_once_the_service_answers_again() {
        let mut m = joined();
        m.apply(ChatEvent::Failed(
            "you have been removed from the chat by the administrator".into(),
        ));
        assert!(m.error.is_some(), "the refusal is shown");

        m.apply(ChatEvent::State {
            consented: true,
            display_name: Some("PA0ABC".into()),
            consent_text_version: 3,
            reports_left: 5,
        });
        assert_eq!(m.error, None, "and it goes when the service answers again");
    }

    /// Without needing the operator to type anything first, which was the old
    /// way out and is not one: somebody who has just been told they are out
    /// does not try again to find out whether they still are.
    #[test]
    fn nobody_has_to_type_to_find_out_the_refusal_is_over() {
        let mut m = joined();
        m.apply(ChatEvent::Failed("no".into()));
        m.apply(ChatEvent::State {
            consented: true,
            display_name: None,
            consent_text_version: 3,
            reports_left: 5,
        });
        assert_eq!(m.error, None);
    }

    /// The path the operator actually walks, which the first fix missed: a
    /// member polls for messages and never asks for state, so mending the
    /// state arm mended the arm that does not run for them.
    #[test]
    fn a_lifted_ban_clears_on_the_next_batch_of_messages() {
        let mut m = joined();
        m.apply(ChatEvent::Failed(
            "you have been removed from the chat by the administrator".into(),
        ));
        assert!(m.error.is_some());

        // Nothing new was said while they were out - an empty batch is still
        // an answer, and it is the commonest one.
        m.apply(ChatEvent::Messages { new: vec![], edited: vec![] });
        assert_eq!(m.error, None, "an answer is an answer, even an empty one");
    }

    /// Polling for answers to a problem report is an answer too.
    #[test]
    fn any_reply_from_the_service_ends_the_refusal() {
        let mut m = joined();
        m.apply(ChatEvent::Failed("no".into()));
        m.apply(ChatEvent::Answers(vec![]));
        assert_eq!(m.error, None);
    }

    /// And the two that must NOT clear it, or a refusal would flash past
    /// unread.
    #[test]
    fn a_refusal_and_a_silence_leave_it_standing() {
        let mut m = joined();
        m.apply(ChatEvent::Failed("out you go".into()));
        m.apply(ChatEvent::Offline(OfflineReason::Unreachable));
        assert_eq!(m.error.as_deref(), Some("out you go"));
    }
}
