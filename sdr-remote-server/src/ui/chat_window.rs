// SPDX-License-Identifier: GPL-2.0-or-later
//
//! The chat, in the server GUI.
//!
//! The window is this file's business; everything inside it belongs to
//! `sdr_remote_chat::ChatPanel`, the same component the desktop client draws.
//! The server is a station in its own right — it runs beside Thetis and drives
//! the radio whether or not anybody has a client open — so it gets the same
//! chat and the same problem reporting, not a smaller version of them.
//!
//! Sharing the component is what keeps them identical. A consent text painted
//! twice is a consent text that ends up saying two things.

use eframe::egui;
use egui::{ViewportBuilder, ViewportId};

use super::ServerApp;

impl ServerApp {
    /// The button, for the top bar. Carries the unread count, because a channel
    /// nobody opens is as empty as no channel at all (design §1.7).
    pub(super) fn render_chat_button(&mut self, ui: &mut egui::Ui) {
        // Shown always, including on a station with no relay at all. It was
        // hidden there for a while, on the rule that an unexplained dead control
        // is worse than none - and that was right while the window could only
        // say "no chat here". Now that it explains what the chat is and how to
        // reach one, the button is the way in rather than a dead end: somebody
        // curious enough to click gets the answer, and the relay gets found by
        // the people who would benefit from it (owner, 2026-08-20).
        let unread = self.chat.unread();
        let label = if unread > 0 {
            format!("{} ({})", rust_i18n::t!("srv_chat"), unread)
        } else {
            rust_i18n::t!("srv_chat").to_string()
        };
        if ui
            .small_button(label)
            .on_hover_text(rust_i18n::t!("srv_chat_hover").to_string())
            .clicked()
        {
            self.show_chat_window = !self.show_chat_window;
        }
    }

    /// Keep the worker fed, and draw the window when it is open.
    ///
    /// The ticket rides in on the relay's ready-reply, next to the UDP token. No
    /// relay, or a relay with no chat behind it, simply means no ticket — a
    /// normal state that the window reports in its own words.
    pub(super) fn chat_tick_and_render(&mut self, ctx: &egui::Context) {
        let ticket = self
            .status_panel_state
            .as_ref()
            .and_then(|s| s.relay_status.get())
            .and_then(|h| h.snapshot().chat_ticket);
        let relay_url = self.relay_url.clone();
        self.chat
            .tick(&relay_url, ticket.as_deref(), self.show_chat_window);

        if !self.show_chat_window {
            return;
        }

        let files = chat_files();
        let sz = self.chat_window_size.unwrap_or([560.0, 620.0]);
        let mut vb = ViewportBuilder::default().with_title(rust_i18n::t!("srv_chat").to_string());
        if !self.chat_window_init_applied {
            // Geometry is kept in SYSTEM points so a saved layout survives a
            // change of UI scale; a ViewportBuilder wants EGUI points, which
            // egui-winit scales by zoom_factor.
            let z = ctx.zoom_factor().max(0.01);
            vb = vb.with_inner_size([sz[0] / z, sz[1] / z]);
            if let Some(pos) = self.chat_window_pos {
                vb = vb.with_position(egui::pos2(pos[0] / z, pos[1] / z));
            }
            self.chat_window_init_applied = true;
            // Written down because a blank chat window has been reported and the
            // difference between a good opening and a blank one is invisible from
            // the outside. This is every number that goes into creating it; the
            // log travels with a problem report, so a next occurrence arrives
            // already measured instead of needing a round of questions.
            log::info!(
                "Chat window: creating at pos {:?}, size {:?} (system points), zoom {:.2}x",
                self.chat_window_pos,
                sz,
                z
            );
        }
        let mut closed = false;
        ctx.show_viewport_immediate(
            ViewportId::from_hash_of("thetislink_chat"),
            vb,
            |ctx, _class| {
                // Back to SYSTEM points on the way in (see the builder above).
                let z = ctx.zoom_factor().max(0.01);
                if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
                    self.chat_window_pos = Some([rect.left() * z, rect.top() * z]);
                }
                if let Some(rect) = ctx.input(|i| i.viewport().inner_rect) {
                    self.chat_window_size = Some([rect.width() * z, rect.height() * z]);
                }
                if ctx.input(|i| i.viewport().close_requested()) {
                    self.show_chat_window = false;
                    self.chat_window_init_applied = false;
                    closed = true;
                    return;
                }
                // No server to ask: this IS the server.
                self.chat.render_body(ctx, &files, &sdr_remote_chat::ServerSide::default());
            },
        );
        // An answer folded away is written down straight away. The server has
        // its own config file and its own copy of the panel, so remembering it
        // in the client and on the phone left this window forgetting on every
        // restart (owner, 2026-08-21).
        if self.chat.take_answers_seen_changed() {
            let ids: Vec<String> = self.chat.seen_ids().iter().map(|i| i.to_string()).collect();
            let joined = ids.join(",");
            crate::config::modify_config(move |c| c.chat_answers_seen = joined);
        }
        if closed {
            self.save_window_positions();
        }
    }
}

/// The server's own log and settings, beside the executable.
///
/// Handed to the component rather than found by it: the client keeps
/// per-profile files under different names, and the shared code should not have
/// to know which host it is drawing for.
fn chat_files() -> sdr_remote_chat::ChatFiles {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    sdr_remote_chat::ChatFiles {
        log: dir.join("thetislink-server.log").to_string_lossy().to_string(),
        conf: dir.join("thetislink-server.conf").to_string_lossy().to_string(),
    }
}
