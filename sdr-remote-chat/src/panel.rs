// SPDX-License-Identifier: GPL-2.0-or-later
//
//! The chat window: the consent screen, or the conversation.
//!
//! Which of the two you see is decided by the service, not by this file — no
//! agreement means no chat (design §6.1), and the rest of ThetisLink carries on
//! untouched either way.
//!
//! Nothing here reaches the network. The worker on its own thread does that
//! (`crate::worker`), and this only drains what it sends back. That is the rule
//! from §2.4 made structural rather than remembered: a chat service that is
//! down cannot make this window slow, because this window never waits on it.

use eframe::egui;
use egui::{Color32, RichText};

use crate::worker::OfflineReason;

/// The colour for a quoted line: clearly secondary, still comfortably readable.
///
/// It used to be small AND weak, which is one step too far. That a line is
/// context rather than the message is already said by the quote mark and the
/// italics; shrinking the type as well left the one line that explains an
/// answer harder to read than the answer itself.
///
/// Halfway between the weak colour egui uses for hints and the ordinary text
/// colour, worked out from the theme in use rather than fixed, so it lands
/// sensibly on a light palette and a dark one alike.
fn quoted_text_color(ui: &egui::Ui) -> Color32 {
    let weak = ui.visuals().weak_text_color();
    let text = ui.visuals().text_color();
    let mix = |a: u8, b: u8| ((a as u16 + b as u16) / 2) as u8;
    Color32::from_rgb(
        mix(weak.r(), text.r()),
        mix(weak.g(), text.g()),
        mix(weak.b(), text.b()),
    )
}

/// How long the edit button stays on one's own message: the service allows
/// fifteen minutes (its `EDIT_WINDOW_SECS`), and this stops offering half a
/// minute earlier - clocks differ a little between machines, and a button that
/// works right up to a boundary it cannot see would sometimes open a field the
/// service is about to refuse. The service remains the judge; this only
/// withdraws the invitation in time.
const EDIT_WINDOW_SHOWN_SECS: i64 = 15 * 60 - 30;

/// A wall-clock time for a message, in the reader's own timezone.
///
/// The service stores and sends UTC seconds, which is the only sane thing for a
/// service; turning that into local time is the reader's business and happens
/// here, once.
fn clock(at: i64) -> String {
    match chrono::DateTime::from_timestamp(at, 0) {
        Some(t) => chrono::DateTime::<chrono::Local>::from(t).format("%H:%M").to_string(),
        // A message with an unreadable time is still a message.
        None => "     ".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::drawable;

    /// A table pasted from a document has to read as a table, not as a wall of
    /// empty boxes. Field report, 2026-08-12.
    #[test]
    fn a_pasted_table_survives_the_default_font() {
        let table = "\u{250C}\u{2500}\u{2500}\u{252C}\u{2500}\u{2510}\n\
                     \u{2502} a \u{2502} b \u{2502}\n\
                     \u{2514}\u{2500}\u{2500}\u{2534}\u{2500}\u{2518}";
        let drawn = drawable(table);
        assert_eq!(drawn, "+--+-+\n| a | b |\n+--+-+");
        assert!(!drawn.chars().any(|c| c as u32 > 0x7F), "nothing left to tofu");
    }

    /// What a word processor does to quotes and dashes, undone.
    #[test]
    fn typographic_punctuation_becomes_plain() {
        assert_eq!(
            drawable("\u{201C}don\u{2019}t\u{201D} \u{2013} see\u{2026}"),
            "\"don't\" - see..."
        );
    }

    /// Ordinary text is left alone, accents and all - latin-1 draws fine.
    #[test]
    fn ordinary_text_is_untouched() {
        assert_eq!(drawable("Hallo Fred, 3.623 MHz - prima!"), "Hallo Fred, 3.623 MHz - prima!");
        assert_eq!(drawable("caf\u{e9} \u{fc}ber"), "caf\u{e9} \u{fc}ber");
    }
}

/// Somebody else's text, in characters this window can actually draw.
///
/// egui's default font is deliberately small and covers little more than
/// latin-1; anything outside it comes out as an empty box. That is a rule this
/// project keeps for its OWN labels (they are written in ASCII), but a chat
/// message is not ours to write - somebody pastes a table from a document and
/// the desktop shows a grid of boxes where a phone, with a full system font,
/// shows the table. Reported from the field on 2026-08-12, after a parameter
/// list was pasted into the chat.
///
/// So the characters that carry meaning are mapped to the ASCII they were drawn
/// from: box-drawing to `|`, `-` and `+`, typographic punctuation to its plain
/// equivalents. Only for display - what was sent stays what was sent, and every
/// other front end still shows the original.
fn drawable(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            // Box drawing (U+2500..U+257F): verticals, horizontals, and every
            // corner or junction, flattened to the three characters a plain
            // font has.
            '\u{2500}'..='\u{2501}' | '\u{2504}'..='\u{2507}' | '\u{2550}' => '-',
            '\u{2502}'..='\u{2503}' | '\u{2506}'..='\u{250B}' | '\u{2551}' => '|',
            '\u{250C}'..='\u{254B}' | '\u{2552}'..='\u{256C}' => '+',
            // Block elements and shades: a filled cell reads as a hash.
            '\u{2580}'..='\u{259F}' => '#',
            // What word processors do to quotes, dashes and dots.
            '\u{2018}' | '\u{2019}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201F}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{2022}' | '\u{00B7}' | '\u{2043}' => '*',
            '\u{00A0}' | '\u{2007}' | '\u{202F}' => ' ',
            other => other,
        })
        .collect::<String>()
        // Two characters wide, so not a char-for-char map.
        .replace('\u{2026}', "...")
        .replace('\u{2192}', "->")
        .replace('\u{2190}', "<-")
}

/// The first words of something, for a quoted line.
///
/// Counted in characters and not bytes: a callsign is ASCII but a message is
/// not, and slicing a string mid-character panics.
fn shorten(text: &str, max: usize) -> String {
    let flat = text.replace('\n', " ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max).collect();
    format!("{}\u{2026}", cut.trim_end())
}

/// How long ago, in words a person reads at a glance.
///
/// Rounded and vague on purpose: the question is never "how many seconds" but
/// "is this from before or after the thing I just did".
fn age_text(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 45 {
        return rust_i18n::t!("chat_diag_age_now").to_string();
    }
    let mins = (secs + 30) / 60;
    if mins < 60 {
        return rust_i18n::t!("chat_diag_age_min", n = mins.to_string()).to_string();
    }
    rust_i18n::t!("chat_diag_age_hour", n = ((mins + 30) / 60).to_string()).to_string()
}

/// Where this front end keeps its own log and settings.
///
/// Handed in rather than worked out, because the two hosts differ: the client
/// writes per-profile files so two instances on one PC each report their own,
/// and the server keeps one pair beside its executable.
pub struct ChatFiles {
    pub log: String,
    pub conf: String,
}

/// The connected server's side of a report, as the host knows it.
///
/// The panel knows nothing about the ThetisLink protocol and must not: it says
/// what it wants and reads what it was given. The host asks the server, waits
/// for the numbered parts, and puts the result here.
#[derive(Default, Clone)]
pub struct ServerSide {
    /// Is there a server to ask at all? False in the server's own GUI - it is
    /// the server - and false in a client that is not connected.
    pub connected: bool,
    /// The cleaned attachment, once every part has arrived.
    pub text: Option<String>,
    /// How a transfer ended when it did not end with a report: (arrived, expected).
    pub failed: Option<(u16, u16)>,
}

/// The chat and problem reporting, with all the state they need.
///
/// Owned by the host application, ticked and drawn by it. Everything that is
/// not chat lives outside: this holds no relay, no ticket and no window.
pub struct ChatPanel {
    /// The chat itself - the worker, what has arrived, and who we are to the
    /// service. Shared with the Android front end, which holds the same model
    /// and draws it with Compose: the rules of the conversation are written
    /// once (see [`crate::ChatModel`]) and only the pixels differ.
    m: crate::model::ChatModel,

    name_input: String,
    /// The age confirmation on the consent screen.
    ///
    /// A box and not a sentence in the manual, which is what the design asked
    /// for and why: under sixteen a parent has to agree, and a line nobody reads
    /// records nothing. Never remembered between visits - an unticked box that
    /// ticks itself is not a confirmation.
    consent_age_ok: bool,
    input: String,
    /// Whether the remove-me window is up.
    leave_open: bool,
    /// The message being answered, while one is. Cleared when the answer goes,
    /// and by the button that takes it back - a reply pointing at something you
    /// have forgotten you selected is worse than no reply at all.
    replying_to: Option<(i64, String, String)>,
    /// The own message being corrected, while one is. The input field holds its
    /// text; Send becomes the correction. Same rules as replying: cleared when
    /// it goes, and cancellable, and the two exclude each other - one input
    /// field, one intent.
    editing: Option<i64>,

    /// An answer was folded away this frame, so the ids want writing down.
    /// Taken by the host, which is the side that owns a config file.
    answers_seen_changed: bool,

    /// The relay address this was last ticked with. Kept so the report knows
    /// which host must never appear in a log.
    relay_url: String,

    // "Report a problem": the description is the report, the log and settings
    // are an attachment with a tick beside it (design §1.1).
    diag_open: bool,
    diag_note: String,
    /// On by default - a description alone rarely settles anything - but nothing
    /// is read from disk until it is ticked.
    diag_attach: bool,
    diag_preview: Option<String>,
    /// When the attachment was read from disk.
    ///
    /// Shown beside the preview, because the form is not modal: leaving it open
    /// while reproducing the fault and then sending is the ordinary way to
    /// attach a log from before the fault. Re-reading at send time would fix
    /// that and break something worth more - design 1.1 step 5 promises that
    /// what is on screen is what goes. So it says how old it is and offers to
    /// read it again, and the choice stays with the person who knows.
    diag_built_at: Option<std::time::Instant>,
    diag_attach_error: Option<String>,
    diag_sent: bool,
    /// Whether the server's log should go with the report. Off by default: it
    /// is somebody else's machine in the general case, and asking for it costs
    /// a transfer.
    diag_attach_server: bool,
    /// Set when the operator ticks that box; the host reads it, asks the
    /// server, and clears it. A flag rather than a callback, because the panel
    /// borrows itself all the way down and a closure would have to borrow it
    /// again.
    want_server_report: bool,
}

impl Default for ChatPanel {
    fn default() -> Self {
        Self {
            m: crate::model::ChatModel::default(),
            name_input: String::new(),
            consent_age_ok: false,
            input: String::new(),
            leave_open: false,
            replying_to: None,
            editing: None,
            answers_seen_changed: false,
            relay_url: String::new(),
            diag_open: false,
            diag_note: String::new(),
            diag_attach: true,
            diag_preview: None,
            diag_built_at: None,
            diag_attach_error: None,
            diag_sent: false,
            // On by default, for the same reason the local one is: nearly
            // every problem worth reporting is a conversation between the two
            // machines, and the half that runs beside the radio is the half
            // nobody can look at afterwards. It stayed off by default, so it
            // was a tick to remember at the moment of writing a description -
            // and a report about receive audio arrived with no server log in
            // it at all, which made the measurements it was written to carry
            // simply absent (2026-08-13). Still previewed, still removable.
            diag_attach_server: true,
            want_server_report: false,
        }
    }
}

impl ChatPanel {
    /// Has the operator asked for the server's log since this was last called?
    ///
    /// Taken rather than read, so the host asks once per tick of the box.
    pub fn take_server_report_request(&mut self) -> bool {
        std::mem::take(&mut self.want_server_report)
    }

    /// How many messages have arrived that nobody has looked at.
    ///
    /// For a badge on whatever button opens this. Reading it costs nothing, so
    /// a host may ask every frame.
    pub fn unread(&self) -> usize {
        self.m.unread
    }

    /// Start the worker thread once, and keep it told where to reach the chat.
    ///
    /// Called every frame; everything in it is cheap and idempotent, because the
    /// alternative is a lifecycle spread over half a dozen call sites.
    /// Called every frame by the host.
    ///
    /// `open` is the host's own flag for whether its window is showing: polling
    /// only happens while somebody is looking, which is what keeps a closed chat
    /// free (design §2.4).
    pub fn tick(&mut self, relay_url: &str, ticket: Option<&str>, open: bool) {
        self.relay_url = relay_url.to_string();
        // The worker, the schedule and what arrives are the model's business;
        // this window only draws what came of it.
        self.m.tick(relay_url, ticket, open);
        // A report that arrived closes its form and says so. This used to be
        // part of draining the events and was lost when that moved into the
        // model: the form then stayed open after a successful send, which reads
        // as "nothing happened" and invites pressing send again - fifteen times,
        // in the case that found it (2026-08-12).
        if self.m.take_diagnosis_sent() {
            self.close_diagnosis();
            self.diag_note.clear();
            self.diag_sent = true;
        }
    }

    fn render_answers(&mut self, ctx: &egui::Context) -> Option<i64> {
        let unread = self.m.unread_answers();
        if unread.is_empty() {
            return None;
        }
        let mut dismissed = None;
        // Bounded and scrollable, because this used to be neither: a plain loop
        // over every unread answer in a top panel, which after a restart meant
        // EVERY answer - the folding away was not remembered - and the panel
        // then took the whole window. The conversation and the report button
        // were squeezed to nothing underneath it and there was no scrollbar, so
        // a reader with a few answers could no longer read the chat and could
        // not report that either (two users, 2026-08-20). A third of the chat
        // window at most, clamped. Note what this does NOT do: it is a fraction
        // of the total height, so nothing reserves a minimum for the
        // conversation - in a small window the header, the input line and this
        // together still take nearly everything. Bounded is not the same as
        // guaranteed room, and the comment used to claim the second.
        let max_h = (ctx.screen_rect().height() / 3.0).clamp(80.0, 260.0);
        egui::TopBottomPanel::top("chat_answers").show(ctx, |ui| {
            ui.add_space(4.0);
            egui::ScrollArea::vertical().max_height(max_h).show(ui, |ui| {
            for a in unread {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(rust_i18n::t!("chat_answer_from_admin").to_string())
                            .strong()
                            .color(Color32::from_rgb(120, 200, 140)),
                    );
                    ui.label(
                        RichText::new(clock(a.at))
                            .small()
                            .monospace()
                            .color(ui.visuals().weak_text_color()),
                    );
                    if ui
                        .small_button("x")
                        .on_hover_text(rust_i18n::t!("chat_answer_dismiss").to_string())
                        .clicked()
                    {
                        dismissed = Some(a.id);
                    }
                });
                ui.label(drawable(&a.body));
                ui.add_space(4.0);
            }
            });
            ui.separator();
        });
        dismissed
    }

    /// The folded-away ids, for the host to write down.
    pub fn seen_ids(&self) -> Vec<i64> {
        self.m.seen_ids()
    }

    /// Take back what the host had written down, at startup.
    pub fn restore_seen(&mut self, ids: &[i64]) {
        self.m.restore_seen(ids);
    }

    /// Have the folded-away ids changed since last asked? Taken, not read: the
    /// host writes them once per change rather than on every frame.
    pub fn take_answers_seen_changed(&mut self) -> bool {
        std::mem::take(&mut self.answers_seen_changed)
    }

    pub fn render_body(&mut self, ctx: &egui::Context, files: &ChatFiles, server: &ServerSide) {
        if let Some(id) = self.render_answers(ctx) {
            self.m.dismiss_answer(id);
            self.answers_seen_changed = true;
        }
        // Drawn first and on every screen: a problem report needs a valid ticket
        // and nothing else, so somebody who never joined the chat - or cannot
        // reach it - can still send one.
        self.render_diagnosis_form(ctx, files, server);
        self.render_leave_window(ctx);

        // The conversation lays itself out with panels (see below), so it is
        // drawn outside the CentralPanel the other two states share.
        if self.m.offline.is_none() && self.m.consented == Some(true) {
            self.render_conversation(ctx, files, server);
            return;
        }
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(why) = self.m.offline {
                // Not an error, and deliberately not styled like one: a relay
                // without a chat behind it is a normal state (§2.4). But it does
                // say WHICH of the three, because they need different things
                // from the reader - and one word covering all three is the fault
                // this project keeps finding in its own reviews.
                ui.add_space(12.0);
                // Each arm names its own key inside `t!` rather than handing a
                // variable to one call. It reads no worse, and it is what lets
                // the test below see which keys this file asks for - the three
                // that went missing were exactly the ones hidden behind a
                // variable.
                let line = match why {
                    OfflineReason::NoRelay => rust_i18n::t!("chat_offline_no_relay"),
                    OfflineReason::NoTicket => rust_i18n::t!("chat_offline_no_ticket"),
                    OfflineReason::Unreachable => rust_i18n::t!("chat_offline_unreachable"),
                };
                // What it is, what a relay does for you, and where to get one -
                // in the same words for all three states rather than three
                // copies drifting apart. One line saying "no chat here" left the
                // reader to guess whether something was broken, whether it was
                // theirs to fix, and what it would have been for. This is the
                // one screen a curious operator reaches on a station with no
                // relay at all, so it is also where the relay itself gets
                // explained rather than assumed.
                //
                // Scrolled: four paragraphs do fit the default window, but not a
                // window somebody has dragged smaller, and text that is silently
                // cut off is worse here than anywhere else.
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.label(line.to_string());
                    // Written out one by one, not looped over an array of key
                    // names: the test in lib.rs only sees keys that appear as a
                    // literal inside `t!`, and a loop would hide these three from
                    // it exactly the way the offline texts were hidden before.
                    let dim = ui.visuals().weak_text_color();
                    for text in [
                        rust_i18n::t!("chat_offline_what_it_is"),
                        rust_i18n::t!("chat_offline_what_a_relay_is"),
                        rust_i18n::t!("chat_offline_get_access"),
                    ] {
                        // A blank line in the translation is a paragraph break,
                        // and the gap is set here rather than in the text: the
                        // four languages then break in the same places and space
                        // them the same way, and a translator cannot make one
                        // read tighter than another by counting newlines.
                        for para in text.split("\n\n") {
                            ui.add_space(14.0);
                            ui.label(RichText::new(para).color(dim));
                        }
                    }
                });
                return;
            }
            match self.m.consented {
                Some(false) => self.render_consent(ui, files, server),
                // Asked, not answered yet. `Some(true)` is handled above.
                _ => {
                    ui.add_space(12.0);
                    ui.label(rust_i18n::t!("chat_connecting").to_string());
                }
            }
        });
    }

    /// The button that opens the report form, for the screens that need it.
    fn render_report_button(&mut self, ui: &mut egui::Ui, files: &ChatFiles, _server: &ServerSide) {
        if ui
            .button(rust_i18n::t!("chat_diag_button").to_string())
            .on_hover_text(rust_i18n::t!("chat_diag_hover").to_string())
            .clicked()
        {
            self.start_diagnosis(files);
        }
        if self.diag_sent {
            ui.label(
                RichText::new(rust_i18n::t!("chat_diag_sent").to_string())
                    .small()
                    .color(egui::Color32::from_rgb(120, 200, 140)),
            );
        }
    }

    /// The consent screen. Its wording lives in the locale file, not here — it
    /// is the one text every user reads once and carefully, and §6.6 of the
    /// design is what it has to match.
    fn render_consent(&mut self, ui: &mut egui::Ui, files: &ChatFiles, server: &ServerSide) {
        ui.add_space(6.0);
        ui.label(RichText::new(rust_i18n::t!("chat_consent_title").to_string()).strong().size(15.0));
        ui.add_space(6.0);
        ui.label(rust_i18n::t!("chat_consent_intro").to_string());
        ui.add_space(10.0);

        ui.label(RichText::new(rust_i18n::t!("chat_consent_name_q").to_string()).strong());
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.name_input)
                    .hint_text(rust_i18n::t!("chat_consent_name_hint").to_string())
                    .desired_width(220.0),
            );
        });
        ui.label(
            RichText::new(rust_i18n::t!("chat_consent_callsign_warning").to_string())
                .small()
                .color(ui.visuals().weak_text_color()),
        );

        ui.add_space(10.0);
        // Who is being agreed with, before what is agreed to. Somebody handing
        // over a name and their messages is entitled to know to whom, and where
        // to go to get it back - and this is the only screen that can say so
        // before the fact rather than after.
        ui.label(rust_i18n::t!("chat_consent_admin").to_string());
        ui.add_space(10.0);
        ui.label(rust_i18n::t!("chat_consent_stored").to_string());
        ui.add_space(6.0);
        ui.label(rust_i18n::t!("chat_consent_withdraw").to_string());
        ui.add_space(6.0);
        // The ban, and it is on this screen because of what the owner decided
        // it costs: it closes the postbox too, and leaving the chat does not
        // lift it. Both of those are things somebody has to be told BEFORE
        // they agree, not discovered afterwards (design 7 + section 6.6).
        ui.label(rust_i18n::t!("chat_consent_ban").to_string());
        ui.label(rust_i18n::t!("chat_consent_access").to_string());
        ui.add_space(10.0);

        ui.checkbox(
            &mut self.consent_age_ok,
            rust_i18n::t!("chat_consent_age").to_string(),
        );
        ui.add_space(8.0);

        // No pre-ticked box and no "by continuing you agree": the button is the
        // act of agreeing, and it does nothing until a name has been chosen and
        // the age confirmed.
        let can = !self.name_input.trim().is_empty() && self.consent_age_ok;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(can, egui::Button::new(rust_i18n::t!("chat_consent_agree").to_string()))
                .clicked()
            {
                let name = self.name_input.trim().to_string();
                self.m.consent(&name);
            }
            if !can {
                let why = if self.name_input.trim().is_empty() {
                    rust_i18n::t!("chat_consent_need_name")
                } else {
                    rust_i18n::t!("chat_consent_need_age")
                };
                ui.label(
                    RichText::new(why.to_string())
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
            }
        });
        ui.add_space(8.0);
        ui.label(
            RichText::new(rust_i18n::t!("chat_consent_optional").to_string())
                .small()
                .color(ui.visuals().weak_text_color()),
        );
        self.render_error(ui);

        // Reporting a problem is not chat: it needs a ticket, not a name, and
        // somebody who has not joined - or does not want to - must still be able
        // to say what is wrong (design section 4).
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(6.0);
        ui.label(
            RichText::new(rust_i18n::t!("chat_diag_without_joining").to_string())
                .small()
                .color(ui.visuals().weak_text_color()),
        );
        ui.horizontal(|ui| {
            self.render_report_button(ui, files, server);
        });
    }

    fn render_conversation(&mut self, ctx: &egui::Context, files: &ChatFiles, server: &ServerSide) {
        // Looking at it is what makes it read.
        self.m.unread = 0;

        // Panels rather than arithmetic. An earlier version worked out the list
        // height from the space left over and put the input inside the same
        // area; the list then grew with its contents instead of scrolling, so
        // anything above the top of the window was simply unreachable. A bottom
        // panel claims its space first and the scroll area gets what remains,
        // which is what makes it a scroll area at all.
        egui::TopBottomPanel::top("chat_head").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(
                        rust_i18n::t!("chat_as_name", name = self.m.display_name.clone())
                            .to_string(),
                    )
                    .small()
                    .color(ui.visuals().weak_text_color()),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    self.render_leave_buttons(ui);
                    ui.separator();
                    self.render_report_button(ui, files, server);
                });
            });
        });

        egui::TopBottomPanel::bottom("chat_input").show(ctx, |ui| {
            self.render_error(ui);
            // What you are answering, right above where you type it - the only
            // place it can be seen at the moment it matters.
            if let Some((_, who, what)) = self.replying_to.clone() {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "> {}: {}",
                            who,
                            drawable(&shorten(&what, 50))
                        ))
                        .italics()
                        .color(quoted_text_color(ui)),
                    );
                    if ui
                        .small_button("x")
                        .on_hover_text(rust_i18n::t!("chat_reply_cancel").to_string())
                        .clicked()
                    {
                        self.replying_to = None;
                    }
                });
            }
            // Correcting rather than composing, said right above where the text
            // is - and cancellable without sending, which throws the draft away
            // and keeps the original.
            if self.editing.is_some() {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(rust_i18n::t!("chat_editing").to_string())
                            .italics()
                            .color(quoted_text_color(ui)),
                    );
                    if ui
                        .small_button("x")
                        .on_hover_text(rust_i18n::t!("chat_edit_cancel").to_string())
                        .clicked()
                    {
                        self.editing = None;
                        self.input.clear();
                    }
                });
            }
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                let send_w = 70.0;
                // The field grows with the text, so a longer message can be read
                // back in full before it goes - a single line loses its own
                // beginning after one clause (user report, 2026-08-12). Beyond
                // about five rows it scrolls instead of eating the window.
                // Enter still sends; a deliberate line break is Shift+Enter.
                let resp = egui::ScrollArea::vertical()
                    .id_salt("chat_input_grow")
                    .max_height(96.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.input)
                                .hint_text(rust_i18n::t!("chat_input_hint").to_string())
                                .desired_rows(1)
                                .return_key(Some(egui::KeyboardShortcut::new(
                                    egui::Modifiers::SHIFT,
                                    egui::Key::Enter,
                                )))
                                .desired_width((ui.available_width() - send_w).max(80.0)),
                        )
                    })
                    .inner;
                let by_key = resp.has_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
                let by_click = ui.button(rust_i18n::t!("chat_send").to_string()).clicked();
                if (by_key || by_click) && !self.input.trim().is_empty() {
                    let body = self.input.trim().to_string();
                    match self.editing {
                        // The same field and the same button carry the
                        // correction; which one this is was said above it.
                        Some(id) => self.m.edit(id, &body),
                        None => {
                            let reply_to = self.replying_to.as_ref().map(|(id, _, _)| *id);
                            self.m.send(&body, reply_to);
                        }
                    }
                    self.replying_to = None;
                    self.editing = None;
                    self.input.clear();
                    resp.request_focus();
                }
            });
            ui.add_space(2.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                // Fill the panel instead of shrinking to the content: a scroll
                // area that shrinks to fit never has anything to scroll.
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let mut answer: Option<(i64, String, String)> = None;
                    let mut start_edit: Option<(i64, String)> = None;
                    for m in &self.m.messages {
                        // What is being answered, above the answer. One line,
                        // shortened: enough to place it, not enough to read the
                        // conversation twice.
                        if let Some(quote) = &m.reply_text {
                            ui.horizontal_wrapped(|ui| {
                                ui.add_space(12.0);
                                ui.label(
                                    // "> " and not an arrow glyph: the default
                                    // egui font renders most arrows as a tofu
                                    // box, and the quote convention is older
                                    // than the arrow anyway.
                                    RichText::new(format!(
                                        "> {}: {}",
                                        m.reply_name.clone().unwrap_or_else(|| {
                                            rust_i18n::t!("chat_left_user").to_string()
                                        }),
                                        drawable(&shorten(quote, 60))
                                    ))
                                    .italics()
                                    .color(quoted_text_color(ui)),
                                );
                            });
                        }
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                RichText::new(clock(m.at))
                                    .small()
                                    .monospace()
                                    .color(ui.visuals().weak_text_color()),
                            );
                            match &m.name {
                                Some(n) => ui.label(RichText::new(format!("{n}:")).strong()),
                                // Somebody who left the chat. Their words stay so
                                // the conversation reads; they do not (§6.4).
                                None => ui.label(
                                    RichText::new(rust_i18n::t!("chat_left_user").to_string())
                                        .italics()
                                        .color(ui.visuals().weak_text_color()),
                                ),
                            };
                            ui.label(drawable(&m.body));
                            if m.edited {
                                // A message that changed after people read it
                                // says so; the marker comes from the service,
                                // not from this window's own memory.
                                ui.label(
                                    RichText::new(rust_i18n::t!("chat_edited").to_string())
                                        .small()
                                        .italics()
                                        .color(ui.visuals().weak_text_color()),
                                );
                            }
                            if ui
                                .small_button(rust_i18n::t!("chat_reply").to_string())
                                .on_hover_text(rust_i18n::t!("chat_reply_hover").to_string())
                                .clicked()
                            {
                                answer = Some((
                                    m.id,
                                    m.name.clone().unwrap_or_else(|| {
                                        rust_i18n::t!("chat_left_user").to_string()
                                    }),
                                    m.body.clone(),
                                ));
                            }
                            // One's own words offer a correction - while they
                            // still can. A button that is shown, opens the
                            // field, and only then says the time has passed is
                            // an invitation withdrawn after it was accepted; it
                            // simply disappears instead. Half a minute before
                            // the service's own limit, so a click racing the
                            // clock is won by the clock - and the service stays
                            // the judge of ownership and window alike.
                            let mine =
                                m.name.as_deref() == Some(self.m.display_name.as_str());
                            let still_editable = mine
                                && chrono::Utc::now().timestamp() - m.at
                                    < EDIT_WINDOW_SHOWN_SECS;
                            if still_editable
                                && ui
                                    .small_button(rust_i18n::t!("chat_edit").to_string())
                                    .on_hover_text(
                                        rust_i18n::t!("chat_edit_hover").to_string(),
                                    )
                                    .clicked()
                            {
                                start_edit = Some((m.id, m.body.clone()));
                            }
                        });
                    }
                    // Set after the loop: the list is borrowed while it runs.
                    if answer.is_some() {
                        self.replying_to = answer;
                        self.editing = None;
                    }
                    if let Some((id, body)) = start_edit {
                        self.editing = Some(id);
                        self.replying_to = None;
                        self.input = body;
                    }
                    if self.m.messages.is_empty() {
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new(rust_i18n::t!("chat_empty").to_string())
                                .color(ui.visuals().weak_text_color()),
                        );
                    }
                });
        });
    }

    /// Report a problem: what you write, and what you choose to send with it.
    ///
    /// The description leads and the log follows, because the description is the
    /// part only the operator can supply and the log is evidence for a story that
    /// has to be told first.
    ///
    /// The attachment is opt-in but asked for plainly, and when it is on, what it
    /// contains is on screen. Design §1.1 makes that the actual safeguard: the
    /// redaction (§1.3) is never complete, so the last thing standing between
    /// somebody's log and somebody else's server is a person reading it.
    fn render_diagnosis_form(&mut self, ctx: &egui::Context, files: &ChatFiles, server: &ServerSide) {
        if !self.diag_open {
            return;
        }
        let mut open = true;
        egui::Window::new(rust_i18n::t!("chat_diag_title").to_string())
            .collapsible(false)
            .resizable(true)
            .default_size([560.0, 460.0])
            .open(&mut open)
            .show(ctx, |ui| {
                // Everything above the buttons scrolls; the button row below
                // stays put. Inside a modest chat window this form can spawn
                // small, and the one control that must never need a window
                // resize to be reached is Send. Scrolling to read is fine;
                // resizing to click is not.
                let body_h = (ui.available_height() - 44.0).max(120.0);
                egui::ScrollArea::vertical()
                    .id_salt("diag_form_body")
                    .max_height(body_h)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                    // The allowance, at the top and before anything is typed.
                    // Finding out at the send button that today is full means
                    // finding out after writing a description and reading a log
                    // - the work is done and the answer is no.
                    if self.m.reports_left == 0 {
                        ui.label(
                            RichText::new(rust_i18n::t!("chat_diag_none_left").to_string())
                                .color(Color32::from_rgb(230, 180, 90)),
                        );
                        ui.add_space(6.0);
                    } else if self.m.reports_left > 0 && self.m.reports_left <= 3 {
                        // Only when it is nearly gone: a number on every visit
                        // is noise, and this one is only interesting near the end.
                        ui.label(
                            RichText::new(
                                rust_i18n::t!(
                                    "chat_diag_left",
                                    n = self.m.reports_left.to_string()
                                )
                                .to_string(),
                            )
                            .small()
                            .color(ui.visuals().weak_text_color()),
                        );
                        ui.add_space(4.0);
                    }
                    ui.label(
                        RichText::new(rust_i18n::t!("chat_diag_note_label").to_string()).strong(),
                    );
                    ui.label(
                        RichText::new(rust_i18n::t!("chat_diag_explain").to_string())
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );
                    ui.add(
                        egui::TextEdit::multiline(&mut self.diag_note)
                            .desired_width(f32::INFINITY)
                            .desired_rows(6)
                            .hint_text(rust_i18n::t!("chat_diag_note_hint").to_string()),
                    );

                    ui.add_space(10.0);
                    ui.separator();

                    // Reading the log costs time and it is not always wanted, so it
                    // happens when the box is ticked and not before.
                    let was = self.diag_attach;
                    ui.checkbox(
                        &mut self.diag_attach,
                        rust_i18n::t!("chat_diag_attach").to_string(),
                    )
                    .on_hover_text(rust_i18n::t!("chat_diag_attach_hint").to_string());
                    if self.diag_attach != was {
                        if self.diag_attach {
                            self.build_attachment(files);
                        } else {
                            self.diag_preview = None;
                            self.diag_attach_error = None;
                        }
                    }
                    ui.label(
                        RichText::new(rust_i18n::t!("chat_diag_attach_hint").to_string())
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );
                    // What becomes of a report, said where the decision is made.
                    //
                    // These four facts used to live only in the consent text, and
                    // reporting does not require consent (design section 4) - so the
                    // very people that rule was written for, the ones who want no
                    // part of the chat, were the ones who never read them.
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(rust_i18n::t!("chat_diag_what_happens").to_string())
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );

                    // The server's own log, when there is a server to ask. Its own
                    // tickbox and its own transfer: it lives on another machine, so
                    // it cannot simply be read the way the local files are.
                    if server.connected {
                        let was = self.diag_attach_server;
                        ui.checkbox(
                            &mut self.diag_attach_server,
                            rust_i18n::t!("chat_diag_attach_server").to_string(),
                        );
                        if self.diag_attach_server && !was {
                            self.want_server_report = true;
                        }
                        if self.diag_attach_server {
                            let line = match (&server.text, server.failed) {
                                (Some(t), _) => rust_i18n::t!(
                                    "chat_diag_server_ready",
                                    kb = (t.len() / 1024).to_string()
                                )
                                .to_string(),
                                // Nothing arrived at all: not a lossy link but a
                                // server that never answered - usually because it
                                // serves one report per address per 20 seconds and
                                // this was asked inside that window. "0 of 0 parts"
                                // reads as a broken link; this says what to do.
                                (None, Some((0, 0))) => {
                                    rust_i18n::t!("chat_diag_server_none").to_string()
                                }
                                // Said with the numbers: "8 of 171 parts" is a
                                // different problem from "0 of 171", and the operator
                                // is the one who knows whether the link is poor.
                                (None, Some((have, parts))) => rust_i18n::t!(
                                    "chat_diag_server_failed",
                                    have = have.to_string(),
                                    parts = parts.to_string()
                                )
                                .to_string(),
                                (None, None) => rust_i18n::t!("chat_diag_server_waiting").to_string(),
                            };
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(line)
                                        .small()
                                        .color(ui.visuals().weak_text_color()),
                                );
                                if server.text.is_none()
                                    && ui.small_button(rust_i18n::t!("chat_diag_reread").to_string()).clicked()
                                {
                                    self.want_server_report = true;
                                }
                            });
                        }
                    }

                    if let Some(why) = self.diag_attach_error.clone() {
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(rust_i18n::t!("chat_diag_attach_failed").to_string())
                                .color(Color32::from_rgb(230, 170, 90)),
                        );
                        ui.label(RichText::new(why).small());
                    }

                    if let Some(server_text) = server.text.clone().filter(|_| self.diag_attach_server) {
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(rust_i18n::t!("chat_diag_server_shown").to_string()).small(),
                        );
                        egui::ScrollArea::vertical()
                            .id_salt("server_attachment")
                            .max_height(140.0)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.add(
                                    egui::TextEdit::multiline(&mut server_text.as_str())
                                        .desired_width(f32::INFINITY)
                                        .font(egui::TextStyle::Monospace)
                                        .interactive(false),
                                );
                            });
                    }

                    if let Some(report) = self.diag_preview.clone() {
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(rust_i18n::t!("chat_diag_attach_shown").to_string()).small(),
                        );
                        // How old it is, and a way to make it current. Without this
                        // the only clue that an attachment predates the fault you
                        // just reproduced is remembering when you opened the form.
                        ui.horizontal(|ui| {
                            let age = self
                                .diag_built_at
                                .map(|t| age_text(t.elapsed()))
                                .unwrap_or_default();
                            ui.label(
                                RichText::new(rust_i18n::t!("chat_diag_read_when", age = age).to_string())
                                    .small()
                                    .color(ui.visuals().weak_text_color()),
                            );
                            if ui
                                .small_button(rust_i18n::t!("chat_diag_reread").to_string())
                                .on_hover_text(rust_i18n::t!("chat_diag_reread_hover").to_string())
                                .clicked()
                            {
                                self.build_attachment(files);
                            }
                        });
                        ui.add_space(4.0);
                        egui::ScrollArea::vertical()
                            .max_height(220.0)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.add(
                                    egui::TextEdit::multiline(&mut report.as_str())
                                        .desired_width(f32::INFINITY)
                                        .font(egui::TextStyle::Monospace)
                                        // Read-only: this is what will be sent, so it
                                        // must not be editable into something else.
                                        .interactive(false),
                                );
                            });
                    }

                    });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    // A report with no description is a log nobody can place, so
                    // the button waits rather than sending one.
                    let told_us = !self.diag_note.trim().is_empty();
                    // And while a ticked server log is still on its way, it
                    // waits for that too. Sending early would quietly drop the
                    // attachment that was asked for - the status line says
                    // "waiting", but a button that works anyway says louder that
                    // waiting is optional. A transfer that FAILED does not hold
                    // the button: that outcome is stated, in the form and in the
                    // report itself.
                    let server_pending = self.diag_attach_server
                        && server.connected
                        && server.text.is_none()
                        && server.failed.is_none();
                    // Today's allowance is gone: the button says so instead of
                    // offering a send that the service will refuse.
                    let none_left = self.m.reports_left == 0;
                    let send = ui
                        .add_enabled(
                            told_us && !server_pending && !none_left,
                            egui::Button::new(rust_i18n::t!("chat_diag_send").to_string()),
                        )
                        .on_disabled_hover_text(if none_left {
                            rust_i18n::t!("chat_diag_none_left").to_string()
                        } else if told_us {
                            rust_i18n::t!("chat_diag_server_wait_send").to_string()
                        } else {
                            rust_i18n::t!("chat_diag_note_needed").to_string()
                        });
                    if send.clicked() {
                        {
                            let mut full = sdr_remote_core::diagnose::describe(
                                &self.diag_note,
                                &self.relay_url,
                                sdr_remote_core::version_string().as_str(),
                                std::env::consts::OS,
                                self.diag_preview.as_deref(),
                            );
                            // Appended rather than merged: two machines, two
                            // logs, and which line came from where is the first
                            // thing anybody reading this needs to know.
                            // The tick is on by default, so a report sent with
                            // no server anywhere must not claim one was asked
                            // for and lost. Text that arrived is always
                            // included, even if the link dropped afterwards.
                            if self.diag_attach_server && (server.connected || server.text.is_some())
                            {
                                if let Some(t) = server.text.as_deref() {
                                    full.push_str("\n\n--- the server, on its own machine ---\n");
                                    full.push_str(t);
                                } else {
                                    // The absence is written down. A report that
                                    // says nothing about a requested log reads as
                                    // if none was asked for. Reached on a failed
                                    // transfer, or when the server went away
                                    // while the box was ticked.
                                    let (have, parts) = server.failed.unwrap_or((0, 0));
                                    full.push_str(&format!(
                                        "\n\n--- the server's log was requested but did not arrive ({have} of {parts} parts) ---\n"
                                    ));
                                }
                            }
                            self.m.send_diagnosis(&full);
                        }
                    }
                    if ui.button(rust_i18n::t!("chat_cancel").to_string()).clicked() {
                        self.close_diagnosis();
                    }
                    if let Some(report) = &self.diag_preview {
                        ui.label(
                            RichText::new(format!("+{} kB", report.len() / 1024))
                                .small()
                                .color(ui.visuals().weak_text_color()),
                        );
                    }
                });
                // A refusal belongs where the send button is. It used to land
                // only in the conversation behind this window: the form stayed
                // open, nothing visibly happened, and the reason - "that is the
                // most reports this station can send in a day" - sat somewhere
                // the sender was not looking (2026-08-12). The form does not
                // close on failure, so this is also what says the report is
                // still here and can be sent again.
                if let Some(err) = self.m.error.clone() {
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new(err).color(Color32::from_rgb(230, 180, 90)),
                        );
                        if ui.small_button("x").clicked() {
                            self.m.error = None;
                        }
                    });
                }
            });
        if !open {
            self.close_diagnosis();
        }
    }

    /// Shut the report form, and throw the attachment away with it.
    ///
    /// Dropping the attachment is the whole point. Closing the form and coming
    /// back later is the ordinary case - you open it, realise you want to
    /// reproduce the fault first, close it, do that, and open it again. Keeping
    /// the old attachment would then send the log from BEFORE the reproduction,
    /// silently: the preview shows exactly what will go, but not that it is
    /// stale. That is design 1.1 step 5 defeated by a detail.
    ///
    /// What was typed stays. Somebody's own words are not worth throwing away,
    /// and they cannot go out of date.
    fn close_diagnosis(&mut self) {
        self.diag_open = false;
        self.diag_preview = None;
        self.diag_built_at = None;
        self.diag_attach_error = None;
    }

    /// Open the form. Reads nothing and sends nothing by itself.
    fn start_diagnosis(&mut self, files: &ChatFiles) {
        self.diag_sent = false;
        self.diag_open = true;
        // Asked for by default, because a description on its own rarely settles
        // anything - but it is a tick the operator can take out.
        if self.diag_attach && self.diag_preview.is_none() {
            self.build_attachment(files);
        }
        // The server's log is asked for again every time this form opens with
        // the box already ticked. It used to be fetched only when the box was
        // ticked - the false-to-true edge - so a second report an hour later
        // carried the log from the first one, labelled "received" and looking
        // entirely fresh. That is the one thing this form must never do: what
        // it says it is sending has to be what it is sending. A report about a
        // standby button was sent that way with a server log from before the
        // test (2026-08-12), which is to say with no evidence in it at all.
        if self.diag_attach_server {
            self.want_server_report = true;
        }
    }

    /// Read the log and settings, cleaned, ready to be looked at.
    ///
    /// A failure here costs the attachment and nothing else: the description is
    /// still worth sending, so it is reported beside the tickbox rather than
    /// thrown as an error over the whole form.
    fn build_attachment(&mut self, files: &ChatFiles) {
        match sdr_remote_core::diagnose::build_attachment(
            &files.log,
            &files.conf,
            &self.relay_url,
        ) {
            Ok(report) => {
                self.diag_preview = Some(report);
                self.diag_built_at = Some(std::time::Instant::now());
                self.diag_attach_error = None;
            }
            Err(e) => {
                self.diag_preview = None;
                self.diag_built_at = None;
                self.diag_attach_error = Some(e.to_string());
            }
        }
    }

    /// One short button in the header bar; the choice itself happens in
    /// [`Self::render_leave_window`].
    ///
    /// It used to be both choices, spelled out, side by side in that bar. Their
    /// labels have to say what they do - one anonymises and the other erases,
    /// and a label that blurs those reads back as "I clicked erase and my
    /// messages are still there" - so they were long, and next to the report
    /// button in a chat window of ordinary width they ran off the end. An
    /// operator had to maximise the window to read his options (2026-08-13).
    /// A bar has room for a word; sentences belong in a window that can wrap
    /// them, which is where the phone has had them all along.
    fn render_leave_buttons(&mut self, ui: &mut egui::Ui) {
        if ui
            .button(rust_i18n::t!("chat_leave").to_string())
            .on_hover_text(rust_i18n::t!("chat_leave_hover").to_string())
            .clicked()
        {
            self.leave_open = true;
        }
    }

    /// What "remove me" means, and the two ways to do it.
    ///
    /// Both are spelled out here rather than one being hidden behind a
    /// confirmation step: the window IS the confirmation, and it can say in
    /// full sentences what a button label cannot - including the thing a
    /// careful operator most needs to hear, that finishing for the day does
    /// not require any of this.
    fn render_leave_window(&mut self, ctx: &egui::Context) {
        if !self.leave_open {
            return;
        }
        let mut open = true;
        egui::Window::new(rust_i18n::t!("chat_leave_title").to_string())
            .collapsible(false)
            .resizable(false)
            .default_width(380.0)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(rust_i18n::t!("chat_leave_explain").to_string());
                ui.add_space(10.0);
                if ui.button(rust_i18n::t!("chat_leave_keep").to_string()).clicked() {
                    self.m.leave(false);
                    self.leave_open = false;
                }
                ui.add_space(4.0);
                if ui
                    .button(
                        RichText::new(rust_i18n::t!("chat_leave_delete").to_string())
                            .color(Color32::from_rgb(230, 110, 110)),
                    )
                    .on_hover_text(rust_i18n::t!("chat_leave_delete_hover").to_string())
                    .clicked()
                {
                    self.m.leave(true);
                    self.leave_open = false;
                }
                ui.add_space(8.0);
                if ui.button(rust_i18n::t!("chat_cancel").to_string()).clicked() {
                    self.leave_open = false;
                }
            });
        if !open {
            self.leave_open = false;
        }
    }

    /// Whatever the service refused, in its own words.
    ///
    /// It explains refusals in language meant for a person (design §8), and
    /// replacing that with something generic here would throw away the only
    /// thing that tells the user what to do differently.
    fn render_error(&mut self, ui: &mut egui::Ui) {
        if let Some(err) = self.m.error.clone() {
            ui.horizontal(|ui| {
                ui.label(RichText::new(err).color(Color32::from_rgb(230, 180, 90)));
                if ui.small_button("x").clicked() {
                    self.m.error = None;
                }
            });
        }
    }
}
