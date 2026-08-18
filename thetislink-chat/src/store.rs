// SPDX-License-Identifier: GPL-2.0-or-later
//
//! Consent and messages, in the chat's own database.
//!
//! Its own — never the relay's `stations.db`, not even read-only (design §2.3).
//! Two processes that update separately must not share a file, or a migration
//! here sits on the relay's data and a hung chat can lock out the thing carrying
//! audio.
//!
//! Everything a user can undo lives here, so the shape of this module is mostly
//! decided by §6.4: leaving the chat has to be able to unpick a person from the
//! rows without unpicking the conversation other people had around them.

use log::{info, warn};
use rusqlite::{params, Connection};

/// Kept in step with §2.2 of the design. A message longer than this is not a
/// question, it is a log fragment - which belongs in a diagnosis report and not
/// in a channel everyone can read (§1.8).
pub const MAX_MESSAGE_CHARS: usize = 2_000;

/// §5: the oldest messages go when the store passes this, and everything goes
/// after 90 days. Two limits because a retention period alone bounds age, not
/// volume.
pub const MAX_MESSAGES: i64 = 200_000;
pub const RETENTION_DAYS: i64 = 90;

/// How many housekeeping rounds are kept for the admin page. Hourly rounds, so
/// this is a fortnight of history in a few kilobytes.
pub const HOUSEKEEPING_KEEP: i64 = 400;

/// One station on the ban list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ban {
    pub station_id: i64,
    pub at: i64,
    pub reason: Option<String>,
    /// Whether that note travels to the station with the refusal.
    pub shared: bool,
    /// The name they go by, if they still have one here. A station that left
    /// the chat keeps its place on the list and loses its name - which is the
    /// deliberate consequence of a ban outliving a withdrawal.
    pub display_name: Option<String>,
}

impl Ban {
    /// What the station is told. The bare fact always; the reason only when
    /// the administrator chose to send it.
    ///
    /// The English here is the fallback. A client that knows this refusal shows
    /// its own translated line instead - the one message that has to be
    /// understood was the only one not in the reader's language.
    pub fn refusal_text(&self) -> String {
        match (&self.reason, self.shared) {
            (Some(r), true) if !r.trim().is_empty() => {
                format!("you have been removed from the chat by the administrator: {r}")
            }
            _ => "you have been removed from the chat by the administrator".to_string(),
        }
    }
}

/// One housekeeping round, as the admin page shows it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Housekeeping {
    pub at: i64,
    pub messages: usize,
    pub reports: usize,
    /// Delivered and never-fetched answers together: the page shows the total
    /// and the log tells them apart, which is where the difference matters.
    pub replies: usize,
    pub markers: usize,
    /// Did the round get through without an error? A failed round used to be
    /// written down as a row of zeros, which on this page is indistinguishable
    /// from the normal quiet round of a young service - the exact distinction
    /// the table was added to make, back one level.
    pub ok: bool,
}

pub struct Store {
    pub(crate) conn: Connection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub id: i64,
    pub at: i64,
    /// What to show. `None` for a message whose author left the chat: the text
    /// stays so the conversation reads, the person does not (§6.4).
    pub display_name: Option<String>,
    pub body: String,
    /// The message this one answers, if any.
    pub reply_to: Option<i64>,
    /// Who wrote that one, and its first words - joined in so a client can show
    /// what is being answered without holding the history.
    pub reply_to_name: Option<String>,
    pub reply_to_excerpt: Option<String>,
    /// When the author corrected it, if ever. Shown as a marker: a message
    /// that changed after people read it should say so.
    pub edited_at: Option<i64>,
}

/// How long a message stays the author's to correct. Long enough to fix a typo
/// spotted on re-reading, short enough that the conversation others answered
/// cannot be rewritten under them.
pub const EDIT_WINDOW_SECS: i64 = 15 * 60;

/// How an edit attempt ends when it does not end in a change.
#[derive(Debug, PartialEq, Eq)]
pub enum EditRefusal {
    NotYours,
    TooOld,
}

impl Store {
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        // A crash must not leave a half-written row behind, and the chat is not
        // latency-critical, so durability beats speed here.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS consent (
                station_id   INTEGER PRIMARY KEY,
                display_name TEXT    NOT NULL,
                text_version INTEGER NOT NULL,
                given_at     INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS messages (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                -- NULL after the author left: the row keeps the conversation
                -- readable while no longer pointing at a person. Nulling the
                -- name but keeping this would leave them traceable through the
                -- relay's own registry, which is the whole point of §6.4.
                station_id   INTEGER,
                display_name TEXT,
                body         TEXT    NOT NULL,
                at           INTEGER NOT NULL,
                -- Which message this one answers, when it answers one. A plain
                -- pointer and not a thread: one level, so a conversation stays
                -- a list you can read from top to bottom.
                reply_to     INTEGER
            );
            CREATE INDEX IF NOT EXISTS messages_at ON messages(at);
            CREATE INDEX IF NOT EXISTS messages_station ON messages(station_id);
            -- The ban list, and it lives here rather than on the relay's
            -- `stations.enabled`: chat policy in the part that must keep
            -- running is the coupling of section 2.3 in another coat, and
            -- no-more-chat would then hang off the same switch as
            -- no-more-relay. Keyed on the station from the ticket, not on a
            -- device, or reinstalling the client would lift it.
            CREATE TABLE IF NOT EXISTS bans (
                station_id INTEGER PRIMARY KEY,
                at         INTEGER NOT NULL,
                -- The administrator's own note. Not shown to the station
                -- unless the flag below says so: what you write to remember
                -- why is shorter and blunter than what you would say to the
                -- person, and the two should not be the same field by accident.
                reason     TEXT,
                shared     INTEGER NOT NULL DEFAULT 0
            );
            -- Every housekeeping round, so it can be watched from the admin
            -- page instead of from the container's log. A round that removes
            -- nothing is written down too: that is the whole point, since on a
            -- service younger than the shortest retention period every round
            -- removes nothing and the silence has to be legible.
            CREATE TABLE IF NOT EXISTS housekeeping (
                id       INTEGER PRIMARY KEY AUTOINCREMENT,
                at       INTEGER NOT NULL,
                messages INTEGER NOT NULL,
                reports  INTEGER NOT NULL,
                replies  INTEGER NOT NULL,
                markers  INTEGER NOT NULL,
                ok       INTEGER NOT NULL DEFAULT 1
            );",
        )?;
        // Databases made before replies existed have no such column, and
        // CREATE TABLE IF NOT EXISTS does nothing for a table that is already
        // there. Asked rather than assumed: adding it twice is an error, and a
        // service that refuses to start over a column is worse than no replies.
        let has_reply_to = conn
            .prepare("SELECT 1 FROM pragma_table_info('messages') WHERE name = 'reply_to'")?
            .exists([])?;
        if !has_reply_to {
            conn.execute("ALTER TABLE messages ADD COLUMN reply_to INTEGER", [])?;
            log::info!("messages.reply_to added to an existing store");
        }
        // The ban list gained a "share this reason" flag one deployment after
        // it was made, so an existing table has to be told about it.
        let has_ok = conn
            .prepare("SELECT 1 FROM pragma_table_info('housekeeping') WHERE name = 'ok'")?
            .exists([])?;
        if !has_ok {
            conn.execute("ALTER TABLE housekeeping ADD COLUMN ok INTEGER NOT NULL DEFAULT 1", [])?;
            log::info!("housekeeping.ok added to an existing store");
        }
        let has_shared = conn
            .prepare("SELECT 1 FROM pragma_table_info('bans') WHERE name = 'shared'")?
            .exists([])?;
        if !has_shared {
            conn.execute("ALTER TABLE bans ADD COLUMN shared INTEGER NOT NULL DEFAULT 0", [])?;
            log::info!("bans.shared added to an existing store");
        }

        // Same story for the edit marker, one deployment later.
        let has_edited_at = conn
            .prepare("SELECT 1 FROM pragma_table_info('messages') WHERE name = 'edited_at'")?
            .exists([])?;
        if !has_edited_at {
            conn.execute("ALTER TABLE messages ADD COLUMN edited_at INTEGER", [])?;
            log::info!("messages.edited_at added to an existing store");
        }

        crate::postbox::init(&conn)?;
        Ok(Self { conn })
    }

    // ---- consent -------------------------------------------------------

    /// Has this station agreed, and under which name?
    pub fn consent_of(&self, station_id: i64) -> rusqlite::Result<Option<String>> {
        let mut q = self
            .conn
            .prepare("SELECT display_name FROM consent WHERE station_id = ?1")?;
        let mut rows = q.query(params![station_id])?;
        Ok(match rows.next()? {
            Some(r) => Some(r.get(0)?),
            None => None,
        })
    }

    /// Record consent. The text version is stored with it, because a text that
    /// changes materially needs asking again and nobody can tell later what
    /// somebody agreed to without it (§6.3).
    pub fn give_consent(
        &self,
        station_id: i64,
        display_name: &str,
        text_version: i64,
        now: i64,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO consent (station_id, display_name, text_version, given_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(station_id) DO UPDATE SET
                display_name = excluded.display_name,
                text_version = excluded.text_version,
                given_at     = excluded.given_at",
            params![station_id, display_name, text_version, now],
        )?;
        info!("station {}: consent given (text version {})", station_id, text_version);
        Ok(())
    }

    /// "Verlaat de chat" — the author is unpicked, the conversation is not.
    ///
    /// Deliberately clears `station_id` as well as the name. Leaving the id
    /// behind would keep every message traceable through the relay's registry,
    /// so the name would be theatre.
    pub fn withdraw(&self, station_id: i64) -> rusqlite::Result<usize> {
        let n = self.conn.execute(
            "UPDATE messages SET display_name = NULL, station_id = NULL WHERE station_id = ?1",
            params![station_id],
        )?;
        self.conn
            .execute("DELETE FROM consent WHERE station_id = ?1", params![station_id])?;
        info!("station {}: left the chat, {} message(s) anonymised", station_id, n);
        Ok(n)
    }

    /// "Verlaat de chat en verwijder mijn berichten" — for the case anonymising
    /// cannot reach: a callsign written into the *text* of a message does not
    /// disappear with the name field (§6.4).
    pub fn withdraw_and_delete(&self, station_id: i64) -> rusqlite::Result<usize> {
        let n = self
            .conn
            .execute("DELETE FROM messages WHERE station_id = ?1", params![station_id])?;
        self.conn
            .execute("DELETE FROM consent WHERE station_id = ?1", params![station_id])?;
        info!("station {}: left the chat, {} message(s) deleted", station_id, n);
        Ok(n)
    }

    // ---- messages ------------------------------------------------------

    pub fn post(
        &self,
        station_id: i64,
        display_name: &str,
        body: &str,
        reply_to: Option<i64>,
        now: i64,
    ) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO messages (station_id, display_name, body, reply_to, at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![station_id, display_name, body, reply_to, now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Correct one's own message, within the window.
    ///
    /// Ownership is the station id, not the name: a name can be re-picked by
    /// somebody else after a leave, a station id cannot. A message whose
    /// station is NULL belongs to somebody who left - nobody may edit it,
    /// its former author included.
    pub fn edit(
        &self,
        station_id: i64,
        id: i64,
        body: &str,
        now: i64,
    ) -> rusqlite::Result<Result<(), EditRefusal>> {
        let row: Option<(Option<i64>, i64)> = self
            .conn
            .query_row(
                "SELECT station_id, at FROM messages WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map(Some)
            .or_else(|e| if e == rusqlite::Error::QueryReturnedNoRows { Ok(None) } else { Err(e) })?;
        let Some((owner, at)) = row else {
            return Ok(Err(EditRefusal::NotYours));
        };
        if owner != Some(station_id) {
            return Ok(Err(EditRefusal::NotYours));
        }
        if now - at > EDIT_WINDOW_SECS {
            return Ok(Err(EditRefusal::TooOld));
        }
        self.conn.execute(
            "UPDATE messages SET body = ?1, edited_at = ?2 WHERE id = ?3",
            params![body, now, id],
        )?;
        Ok(Ok(()))
    }

    /// Everything after `since`, oldest first, capped so one call cannot ask for
    /// the whole store.
    pub fn since(&self, since: i64, limit: i64) -> rusqlite::Result<Vec<Message>> {
        // The answered message is joined in rather than looked up afterwards, so
        // a client can render a reply without holding the history it refers to -
        // which it often will not, since it only ever asked for what is new.
        //
        // A LEFT join: the message being answered may have been pruned or
        // deleted, and a reply to something gone is still a message.
        let mut q = self.conn.prepare(
            "SELECT m.id, m.at, m.display_name, m.body, m.reply_to,
                    p.display_name, substr(p.body, 1, 80), m.edited_at
             FROM messages m
             LEFT JOIN messages p ON p.id = m.reply_to
             WHERE m.id > ?1 ORDER BY m.id ASC LIMIT ?2",
        )?;
        let rows = q.query_map(params![since, limit], |r| {
            Ok(Message {
                id: r.get(0)?,
                at: r.get(1)?,
                display_name: r.get(2)?,
                body: r.get(3)?,
                reply_to: r.get(4)?,
                reply_to_name: r.get(5)?,
                reply_to_excerpt: r.get(6)?,
                edited_at: r.get(7)?,
            })
        })?;
        rows.collect()
    }

    /// Messages corrected after `cutoff`, whatever their id. A client only ever
    /// asks for ids beyond what it holds, so a correction to something it
    /// already shows has to travel separately - carried on every poll for a
    /// while, because polls are cheap and a missed one must not mean a stale
    /// message on somebody's screen for the rest of the evening.
    pub fn edited_after(&self, cutoff: i64, limit: i64) -> rusqlite::Result<Vec<Message>> {
        let mut q = self.conn.prepare(
            "SELECT m.id, m.at, m.display_name, m.body, m.reply_to,
                    p.display_name, substr(p.body, 1, 80), m.edited_at
             FROM messages m
             LEFT JOIN messages p ON p.id = m.reply_to
             WHERE m.edited_at IS NOT NULL AND m.edited_at > ?1
             ORDER BY m.id ASC LIMIT ?2",
        )?;
        let rows = q.query_map(params![cutoff, limit], |r| {
            Ok(Message {
                id: r.get(0)?,
                at: r.get(1)?,
                display_name: r.get(2)?,
                body: r.get(3)?,
                reply_to: r.get(4)?,
                reply_to_name: r.get(5)?,
                reply_to_excerpt: r.get(6)?,
                edited_at: r.get(7)?,
            })
        })?;
        rows.collect()
    }

    /// Drop what is too old or too much, and say so.
    ///
    /// The logging is not decoration: this deletes messages belonging to people
    /// who did nothing, and an event that happens silently to a third party is
    /// the kind that comes back as a fault report nobody can place (§8).
    pub fn prune(&self, now: i64) -> rusqlite::Result<(usize, usize)> {
        let (by_age, by_size) = Self::expire(&self.conn, now)?;

        if by_age > 0 {
            info!("pruned {} message(s) older than {} days", by_age, RETENTION_DAYS);
        }
        if by_size > 0 {
            // Worth a warning rather than an info: this is the store being full,
            // and it removes other people's messages to make room.
            warn!(
                "pruned {} oldest message(s): the store passed {} messages",
                by_size, MAX_MESSAGES
            );
        }
        Ok((by_age, by_size))
    }

    /// What `prune` would remove at `at`, without removing it.
    ///
    /// The real thing inside a transaction that is rolled back - see
    /// `postbox::preview` for why this is not a second set of counting
    /// queries. Here the size limit makes it plainer still: what the ceiling
    /// removes depends on how many rows the age limit took first, so a count
    /// taken beforehand would answer a question nobody asked.
    pub fn preview(&self, at: i64) -> rusqlite::Result<(usize, usize)> {
        let tx = self.conn.unchecked_transaction()?;
        let counts = Self::expire(&tx, at)?;
        tx.rollback()?;
        Ok(counts)
    }

    /// The last stretch of conversation with the station behind each message,
    /// for the administrator only.
    ///
    /// The station number is not in `Message` and must not be: that type goes
    /// to every client, and who wrote what is exactly what section 6.4 keeps
    /// out of it. Here it is needed for one thing - the button that bans the
    /// author - so it travels beside the message rather than inside it, and
    /// only down a route that has already checked it is the administrator.
    ///
    /// `None` for a message whose author left: the text stays, the person does
    /// not, and there is nobody left to ban.
    pub fn recent_with_stations(&self, limit: i64) -> rusqlite::Result<Vec<(Message, Option<i64>)>> {
        let mut q = self.conn.prepare(
            "SELECT id, at, display_name, body, reply_to, edited_at, station_id
             FROM messages ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = q.query_map(params![limit], |r| {
            Ok((
                Message {
                    id: r.get(0)?,
                    at: r.get(1)?,
                    display_name: r.get(2)?,
                    body: r.get(3)?,
                    reply_to: r.get(4)?,
                    reply_to_name: None,
                    reply_to_excerpt: None,
                    edited_at: r.get(5)?,
                },
                r.get(6)?,
            ))
        })?;
        rows.collect()
    }

    // ---- the ban list ---------------------------------------------------

    /// Is this station banned?
    ///
    /// Asked on every request rather than when a ticket is handed out, which
    /// is what makes a ticket issued before the ban stop working - for reading
    /// as much as for writing. A check at issue time would leave whoever was
    /// already holding one inside until it expired.
    pub fn is_banned(&self, station_id: i64) -> rusqlite::Result<bool> {
        Ok(self.ban_of(station_id)?.is_some())
    }

    /// The ban itself, so the refusal can carry what the administrator chose
    /// to say.
    pub fn ban_of(&self, station_id: i64) -> rusqlite::Result<Option<Ban>> {
        let mut q = self.conn.prepare(
            "SELECT b.station_id, b.at, b.reason, b.shared, c.display_name
             FROM bans b LEFT JOIN consent c ON c.station_id = b.station_id
             WHERE b.station_id = ?1",
        )?;
        let mut rows = q.query(params![station_id])?;
        Ok(match rows.next()? {
            Some(r) => Some(Ban {
                station_id: r.get(0)?,
                at: r.get(1)?,
                reason: r.get(2)?,
                shared: r.get::<_, i64>(3)? != 0,
                display_name: r.get(4)?,
            }),
            None => None,
        })
    }

    /// Put a station on the list, or refresh the note on one already there.
    pub fn ban(
        &self,
        station_id: i64,
        reason: Option<&str>,
        shared: bool,
        now: i64,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO bans (station_id, at, reason, shared) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(station_id) DO UPDATE SET
                at = excluded.at,
                -- A second ban without a note keeps the first note rather than
                -- wiping it: the history of why is the only thing here that
                -- cannot be reconstructed afterwards.
                reason = COALESCE(excluded.reason, bans.reason),
                shared = excluded.shared",
            params![station_id, now, reason, if shared { 1 } else { 0 }],
        )?;
        // The reason goes in the line too. `unban` deletes the row, so without
        // this the log is the only place that still knows there was ever a ban
        // and what it was for.
        warn!(
            "station {}: banned from the chat ({})",
            station_id,
            reason.unwrap_or("no reason given")
        );
        Ok(())
    }

    /// Take a station off the list again. A ban is a measure, not a verdict.
    pub fn unban(&self, station_id: i64) -> rusqlite::Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM bans WHERE station_id = ?1", params![station_id])?;
        if n > 0 {
            info!("station {}: ban lifted", station_id);
        }
        Ok(n > 0)
    }

    /// Who is on the list, newest first, with the name they had if it is still
    /// known. A station that left the chat keeps its place and loses its name.
    pub fn bans(&self) -> rusqlite::Result<Vec<Ban>> {
        let mut q = self.conn.prepare(
            "SELECT b.station_id, b.at, b.reason, b.shared, c.display_name
             FROM bans b LEFT JOIN consent c ON c.station_id = b.station_id
             ORDER BY b.at DESC",
        )?;
        let rows = q.query_map([], |r| {
            Ok(Ban {
                station_id: r.get(0)?,
                at: r.get(1)?,
                reason: r.get(2)?,
                shared: r.get::<_, i64>(3)? != 0,
                display_name: r.get(4)?,
            })
        })?;
        rows.collect()
    }

    /// Write down what a housekeeping round did, and keep the recent history
    /// bounded.
    ///
    /// Bounded here rather than in `prune`, because this table is the record
    /// of the pruning and a record that needs pruning to stay honest is one
    /// more thing to get wrong. A few hundred rows is a fortnight of hourly
    /// rounds and a few kilobytes.
    pub fn record_housekeeping(&self, run: Housekeeping) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO housekeeping (at, messages, reports, replies, markers, ok)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![run.at, run.messages as i64, run.reports as i64, run.replies as i64,
                    run.markers as i64, if run.ok { 1 } else { 0 }],
        )?;
        self.conn.execute(
            "DELETE FROM housekeeping WHERE id NOT IN
             (SELECT id FROM housekeeping ORDER BY id DESC LIMIT ?1)",
            params![HOUSEKEEPING_KEEP],
        )?;
        Ok(())
    }

    /// The most recent rounds, newest first.
    pub fn housekeeping_runs(&self, limit: i64) -> rusqlite::Result<Vec<Housekeeping>> {
        let mut q = self.conn.prepare(
            "SELECT at, messages, reports, replies, markers, ok
             FROM housekeeping ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = q.query_map(params![limit], |r| {
            Ok(Housekeeping {
                at: r.get(0)?,
                messages: r.get::<_, i64>(1)? as usize,
                reports: r.get::<_, i64>(2)? as usize,
                replies: r.get::<_, i64>(3)? as usize,
                markers: r.get::<_, i64>(4)? as usize,
                ok: r.get::<_, i64>(5)? != 0,
            })
        })?;
        rows.collect()
    }

    /// The deletions themselves, without a word. Shared by the real run and
    /// the preview so there is one place where the period is decided.
    fn expire(conn: &Connection, now: i64) -> rusqlite::Result<(usize, usize)> {
        let cutoff = now - RETENTION_DAYS * 86_400;
        let by_age = conn.execute("DELETE FROM messages WHERE at < ?1", params![cutoff])?;

        let count: i64 = conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
        let by_size = if count > MAX_MESSAGES {
            let excess = count - MAX_MESSAGES;
            conn.execute(
                "DELETE FROM messages WHERE id IN
                 (SELECT id FROM messages ORDER BY id ASC LIMIT ?1)",
                params![excess],
            )?
        } else {
            0
        };
        Ok((by_age, by_size))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open(":memory:").expect("in-memory store opens")
    }

    const T: i64 = 1_700_000_000;

    #[test]
    fn consent_is_recorded_and_read_back() {
        let s = store();
        assert_eq!(s.consent_of(7).unwrap(), None);
        s.give_consent(7, "PA0ABC", 1, T).unwrap();
        assert_eq!(s.consent_of(7).unwrap().as_deref(), Some("PA0ABC"));
    }

    /// Agreeing again with a different name replaces, rather than piling up a
    /// second row that nothing would ever read.
    #[test]
    fn consenting_again_replaces() {
        let s = store();
        s.give_consent(7, "PA0ABC", 1, T).unwrap();
        s.give_consent(7, "Jan", 1, T + 10).unwrap();
        assert_eq!(s.consent_of(7).unwrap().as_deref(), Some("Jan"));
    }

    #[test]
    fn messages_come_back_in_order_and_only_after_the_marker() {
        let s = store();
        let a = s.post(7, "PA0ABC", "eerste", None, T).unwrap();
        let b = s.post(8, "PA1XYZ", "tweede", None, T + 1).unwrap();
        let all = s.since(0, 100).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, a);
        assert_eq!(all[1].id, b);
        assert_eq!(s.since(a, 100).unwrap().len(), 1, "only what came after");
    }

    /// The heart of §6.4: the text survives, the person does not.
    #[test]
    fn leaving_the_chat_keeps_the_conversation_and_drops_the_author() {
        let s = store();
        s.give_consent(7, "PA0ABC", 1, T).unwrap();
        s.post(7, "PA0ABC", "ik heb dit ook", None, T).unwrap();
        s.post(8, "PA1XYZ", "en ik antwoord erop", None, T + 1).unwrap();

        assert_eq!(s.withdraw(7).unwrap(), 1);

        let all = s.since(0, 100).unwrap();
        assert_eq!(all.len(), 2, "the other person's reply still makes sense");
        assert_eq!(all[0].display_name, None);
        assert_eq!(all[0].body, "ik heb dit ook", "the text stays");
        assert_eq!(all[1].display_name.as_deref(), Some("PA1XYZ"));
        assert_eq!(s.consent_of(7).unwrap(), None, "consent is gone too");
    }

    /// Clearing the name while keeping the station id would leave every message
    /// traceable through the relay's own registry. That is the trap §6.4 warns
    /// about by name, so it gets its own test.
    #[test]
    fn leaving_also_drops_the_station_id_not_just_the_name() {
        let s = store();
        s.post(7, "PA0ABC", "hallo", None, T).unwrap();
        s.withdraw(7).unwrap();
        let left: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM messages WHERE station_id IS NOT NULL", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(left, 0, "nothing may still point at a station");
    }

    /// The second button, for what anonymising cannot reach: a callsign written
    /// into the text itself.
    #[test]
    fn the_second_button_removes_the_text_as_well() {
        let s = store();
        s.post(7, "PA0ABC", "hier PA0ABC, zelfde probleem", None, T).unwrap();
        s.post(8, "PA1XYZ", "iets anders", None, T).unwrap();
        assert_eq!(s.withdraw_and_delete(7).unwrap(), 1);
        let all = s.since(0, 100).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].display_name.as_deref(), Some("PA1XYZ"));
    }

    #[test]
    fn old_messages_are_pruned_and_recent_ones_are_not() {
        let s = store();
        let old = T - (RETENTION_DAYS + 1) * 86_400;
        s.post(7, "PA0ABC", "oud", None, old).unwrap();
        s.post(7, "PA0ABC", "vers", None, T).unwrap();
        let (by_age, _) = s.prune(T).unwrap();
        assert_eq!(by_age, 1);
        assert_eq!(s.since(0, 100).unwrap().len(), 1);
    }

    /// Nothing to do must do nothing - a prune that always deletes something
    /// would quietly eat a quiet channel.
    /// The same guarantee the postbox preview rests on: what it reports is
    /// what a real run then does.
    #[test]
    fn the_preview_is_what_pruning_then_removes() {
        let s = store();
        let old = T - (RETENTION_DAYS + 1) * 86_400;
        s.post(7, "PA0ABC", "oud", None, old).unwrap();
        s.post(8, "PA0DEF", "ouder", None, old - 86_400).unwrap();
        s.post(9, "PA0GHI", "vers", None, T).unwrap();

        let foreseen = s.preview(T).unwrap();
        let done = s.prune(T).unwrap();
        assert_eq!(foreseen, done, "the preview promised something else");
        assert_eq!(done.0, 2);
    }

    /// Asking about a date that has not arrived, and leaving the store alone.
    #[test]
    fn a_preview_looks_ahead_and_removes_nothing() {
        let s = store();
        s.post(7, "PA0ABC", "vandaag", None, T).unwrap();
        assert_eq!(s.preview(T).unwrap(), (0, 0));
        assert_eq!(s.preview(T + (RETENTION_DAYS + 1) * 86_400).unwrap(), (1, 0));
        // Twice, because a rollback that only works once is worse than none.
        assert_eq!(s.preview(T + (RETENTION_DAYS + 1) * 86_400).unwrap(), (1, 0));
        assert_eq!(s.prune(T).unwrap(), (0, 0));
    }

    #[test]
    fn pruning_an_untouched_store_removes_nothing() {
        let s = store();
        s.post(7, "PA0ABC", "vers", None, T).unwrap();
        assert_eq!(s.prune(T).unwrap(), (0, 0));
        assert_eq!(s.since(0, 100).unwrap().len(), 1);
    }
}
