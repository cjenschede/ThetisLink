// SPDX-License-Identifier: GPL-2.0-or-later
//
//! The diagnosis postbox: reports come in, the administrator takes them out.
//!
//! A postbox and not an archive (design §1.4). A report stays only until it has
//! been collected, which keeps the VPS empty, keeps other people's data off
//! somebody else's server, and lets whoever studies a report do it locally —
//! reading one means searching it several times, and pulling it off a machine
//! that is carrying audio every time is the wrong place to do that.
//!
//! Collecting is two-phase on purpose. Deleting at the moment of download reads
//! as tidy and loses the only copy when the download fails, so a report is
//! claimed first and released only once somebody says what happened to it.

use log::{info, warn};
use rusqlite::{params, Connection};

/// §1.2. A report is a log tail plus settings, and since the client can bring
/// the server's log as well it is two of those in one body.
///
/// Half a megabyte looked generous when a report was one machine's log. It is
/// not: a client log, a server log, two sets of settings and JSON escaping on
/// top of all of it can pass that, and when it did the report simply did not
/// arrive - refused before it ever reached this file, silently (2026-08-12).
/// Four megabytes is far more than any of those add up to, and still small
/// against the postbox ceiling below and against a machine with no swap.
pub const MAX_REPORT_BYTES: usize = 4 * 1024 * 1024;

/// §1.2. Enough for a bad day, few enough that the postbox cannot be filled by
/// one station.
///
/// Raised from five once the server GUI got the same window as the client, and
/// from fifteen once Android made three front ends behind one station id.
///
/// Fifteen was still an evening: one station testing across three front ends
/// reached it in an afternoon and then could not report the fault it was
/// testing for (2026-08-12). The number was guarding the wrong thing anyway -
/// the real backstop against a station filling the postbox is
/// `MAX_POSTBOX_BYTES`, which counts what is actually held rather than how
/// often somebody pressed send.
///
/// Reports that have been collected no longer count - `release` deletes them -
/// so an emptied postbox restores the allowance immediately.
pub const MAX_REPORTS_PER_STATION_PER_DAY: i64 = 100;

/// §1.2. The postbox holds what has not been collected yet, so this is a ceiling
/// on a backlog rather than on an archive.
///
/// Raised with the per-report limit: twenty megabytes was five full reports
/// under the new ceiling, which is a backlog one bad evening can produce.
pub const MAX_POSTBOX_BYTES: i64 = 200 * 1024 * 1024;

/// §1.4. How long a claim holds a report before it falls back to unclaimed.
///
/// Long enough to read and save one, short enough that a browser closed midway
/// does not strand it.
pub const CLAIM_LEASE_SECS: i64 = 900;

/// §1.4. Not a retention period - a report that nobody collects should not sit
/// there forever.
pub const UNCOLLECTED_MAX_DAYS: i64 = 30;

/// How long a delivered answer is kept.
///
/// Shorter than a report, because its work is done the moment it is read: the
/// sender has it on their own screen. Long enough that a client reinstalled the
/// same week still sees it.
pub const DELIVERED_REPLY_MAX_DAYS: i64 = 7;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub id: i64,
    pub station_id: Option<i64>,
    pub display_name: Option<String>,
    pub at: i64,
    pub bytes: i64,
    /// Who holds it, and until when. `None` = free to claim.
    pub claimed_until: Option<i64>,
    pub replied: bool,
    /// What was answered, and when it was sent.
    ///
    /// The page could say THAT an answer went out and never what it said, so
    /// the one person who cannot look it up was the one who wrote it - while
    /// the reader still has it on their own screen. That is an odd side to be
    /// on in a conversation you are conducting (2026-08-16).
    pub reply: Option<String>,
    pub reply_at: Option<i64>,
    /// When it was fetched to the administrator's own computer, if it was.
    ///
    /// A collected report is gone from here - that is what collecting means -
    /// but answering one is the ordinary case, and the page used to drop it off
    /// the list the moment it was collected. So the one route that could still
    /// answer it was the command line, which is not where the person answering
    /// is (2026-08-17). There is no body to show and no name kept: only that it
    /// existed, whose it was, and whether it has been answered.
    pub collected_at: Option<i64>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PostboxError {
    TooLarge,
    TooMany,
    Full,
    NotFound,
    /// Releasing something that was never claimed, or claimed by another.
    NotClaimed,
    /// One answer per report: this stays an answer and does not become a
    /// conversation (design section 1.5).
    AlreadyReplied,
}

impl std::fmt::Display for PostboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // These reach the sender, not just the log: "I cannot send anything" is
        // the same symptom for all of them and needs different answers.
        f.write_str(match self {
            PostboxError::TooLarge => "that report is too large",
            // Says the number and what changes it. "several" told nobody
            // whether to wait a minute or a day (design section 8).
            PostboxError::TooMany => {
                "that is the most reports this station can send in a day - they free up                  again as they are collected, or after a day"
            }
            PostboxError::Full => "the postbox is full - try again later",
            PostboxError::NotFound => "no such report",
            PostboxError::NotClaimed => "that report is not claimed",
            PostboxError::AlreadyReplied => "that report has already been answered",
        })
    }
}

pub fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS diagnoses (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            -- NULL after the sender left the chat: an uncollected report of
            -- theirs goes with them (design §6.4), so this only survives for
            -- one already collected, which no longer lives here anyway.
            station_id    INTEGER,
            display_name  TEXT,
            at            INTEGER NOT NULL,
            body          TEXT    NOT NULL,
            bytes         INTEGER NOT NULL,
            claimed_until INTEGER,
            reply         TEXT,
            reply_at      INTEGER
        );
        CREATE INDEX IF NOT EXISTS diagnoses_station ON diagnoses(station_id);

        -- An answer used to live in two columns on the report it answered, and
        -- that put two promises against each other: collecting a report deletes
        -- it (section 1.4, the VPS is a postbox and not an archive), so the
        -- answer went out with it and the sender waited for something that had
        -- been written AND thrown away. Keeping the row instead would have kept
        -- the whole report body on the server, which is the promise the other
        -- way round. It is addressed to a station, not to a row, so it lives
        -- here.
        CREATE TABLE IF NOT EXISTS replies (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            station_id    INTEGER NOT NULL,
            diagnosis_id  INTEGER NOT NULL,
            body          TEXT    NOT NULL,
            at            INTEGER NOT NULL,
            -- When the station fetched it. Fetching is delivery: it is the only
            -- moment this service can observe, and an answer nobody collected
            -- must not be swept up with one that arrived.
            delivered_at  INTEGER
        );
        CREATE INDEX IF NOT EXISTS replies_station ON replies(station_id);

        -- What is left of a collected report: which station it came from, and
        -- when. No name, no body, nothing to read. It exists so an answer can
        -- still be addressed after collection - the documented way of working
        -- fetches and releases in one step, and replying afterwards used to fail
        -- with 'not found' and no explanation.
        CREATE TABLE IF NOT EXISTS collected (
            diagnosis_id  INTEGER PRIMARY KEY,
            station_id    INTEGER NOT NULL,
            at            INTEGER NOT NULL
        );",
    )
}

/// Take a report in, or say why not.
/// How many more reports this station may send today.
///
/// The same count `submit` refuses on, asked one step earlier so a form can say
/// so before it is filled in. Never negative: a limit that was lowered under a
/// station's feet should read as "none left", not as a negative number.
pub fn remaining_today(conn: &Connection, station_id: i64, now: i64) -> i64 {
    let today: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM diagnoses WHERE station_id = ?1 AND at > ?2",
            params![station_id, now - 86_400],
            |r| r.get(0),
        )
        .unwrap_or(0);
    (MAX_REPORTS_PER_STATION_PER_DAY - today).max(0)
}

pub fn submit(
    conn: &Connection,
    station_id: i64,
    // The name they chose to show in the chat, when they are in it. A report
    // does not require one (design section 4), so this is optional.
    display_name: Option<&str>,
    body: &str,
    now: i64,
) -> Result<i64, PostboxError> {
    let bytes = body.len() as i64;
    if bytes as usize > MAX_REPORT_BYTES {
        return Err(PostboxError::TooLarge);
    }

    let day_ago = now - 86_400;
    let today: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM diagnoses WHERE station_id = ?1 AND at > ?2",
            params![station_id, day_ago],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if today >= MAX_REPORTS_PER_STATION_PER_DAY {
        return Err(PostboxError::TooMany);
    }

    // The postbox holds a backlog, so a full one means the administrator has not
    // emptied it - refusing is right, and dropping somebody else's uncollected
    // report to make room would be exactly the silent third-party loss the
    // design warns about.
    let held: i64 = conn
        .query_row("SELECT COALESCE(SUM(bytes), 0) FROM diagnoses", [], |r| r.get(0))
        .unwrap_or(0);
    if held + bytes > MAX_POSTBOX_BYTES {
        warn!("postbox full ({} bytes held): a report was refused", held);
        return Err(PostboxError::Full);
    }

    conn.execute(
        "INSERT INTO diagnoses (station_id, display_name, at, body, bytes) VALUES (?1,?2,?3,?4,?5)",
        params![station_id, display_name, now, body, bytes],
    )
    .map_err(|_| PostboxError::Full)?;
    let id = conn.last_insert_rowid();
    info!("station {}: diagnosis {} received ({} bytes)", station_id, id, bytes);
    Ok(id)
}

/// What is waiting, newest first. Bodies are not included: a listing should not
/// drag megabytes about.
pub fn list(conn: &Connection, now: i64) -> rusqlite::Result<Vec<Report>> {
    let mut q = conn.prepare(
        "SELECT d.id, d.station_id, d.display_name, d.at, d.bytes, d.claimed_until,
                EXISTS(SELECT 1 FROM replies r WHERE r.diagnosis_id = d.id),
                (SELECT r.body FROM replies r WHERE r.diagnosis_id = d.id
                   ORDER BY r.at DESC LIMIT 1),
                (SELECT r.at   FROM replies r WHERE r.diagnosis_id = d.id
                   ORDER BY r.at DESC LIMIT 1)
         FROM diagnoses d ORDER BY d.id DESC LIMIT 200",
    )?;

    // Answers written before they had a table of their own live in two columns
    // on the report. Carry them across rather than leave them where nothing
    // reads them: an answer that was written and is now invisible is the same
    // failure as one that was deleted, and it is somebody's real answer.
    let had_reply_column = conn
        .prepare("SELECT 1 FROM pragma_table_info('diagnoses') WHERE name = 'reply'")?
        .exists([])?;
    if had_reply_column {
        let moved = conn.execute(
            "INSERT INTO replies (station_id, diagnosis_id, body, at)
             SELECT station_id, id, reply, COALESCE(reply_at, at) FROM diagnoses
             WHERE reply IS NOT NULL AND station_id IS NOT NULL
               AND id NOT IN (SELECT diagnosis_id FROM replies)",
            [],
        )?;
        if moved > 0 {
            info!("{} answer(s) moved out of the report rows into their own table", moved);
            conn.execute("UPDATE diagnoses SET reply = NULL, reply_at = NULL", [])?;
        }
    }
    let rows = q.query_map([], |r| {
        let claimed: Option<i64> = r.get(5)?;
        Ok(Report {
            id: r.get(0)?,
            station_id: r.get(1)?,
            display_name: r.get(2)?,
            at: r.get(3)?,
            bytes: r.get(4)?,
            // An expired lease is not a claim: it fell back to free.
            claimed_until: claimed.filter(|until| *until > now),
            replied: r.get(6)?,
            reply: r.get(7)?,
            reply_at: r.get(8)?,
            collected_at: None,
        })
    })?;
    let mut all: Vec<Report> = rows.collect::<rusqlite::Result<Vec<_>>>()?;

    // And what has been collected. The name comes from the consent table rather
    // than from a copy kept here: the marker deliberately holds no name, and
    // this asks the one place that already has one for its own reasons. A
    // station that has left the chat has none, and then it stays blank.
    // The name lives in a table this module does not own, and does not always
    // exist beside it - the postbox has its own schema and its own tests. Asked
    // for only when it is there, so a listing never fails over a nicety.
    let has_consent: bool = conn
        .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'consent'")?
        .exists([])?;
    let name_expr = if has_consent {
        "(SELECT n.display_name FROM consent n WHERE n.station_id = c.station_id)"
    } else {
        "NULL"
    };
    let mut c = conn.prepare(&format!(
        "SELECT c.diagnosis_id, c.station_id, c.at,
                {name_expr},
                EXISTS(SELECT 1 FROM replies r WHERE r.diagnosis_id = c.diagnosis_id),
                (SELECT r.body FROM replies r WHERE r.diagnosis_id = c.diagnosis_id
                   ORDER BY r.at DESC LIMIT 1),
                (SELECT r.at   FROM replies r WHERE r.diagnosis_id = c.diagnosis_id
                   ORDER BY r.at DESC LIMIT 1)
         FROM collected c
         WHERE c.diagnosis_id NOT IN (SELECT id FROM diagnoses)
         ORDER BY c.diagnosis_id DESC LIMIT 200"
    ))?;
    let collected = c.query_map([], |r| {
        Ok(Report {
            id: r.get(0)?,
            station_id: r.get(1)?,
            display_name: r.get(3)?,
            at: r.get(2)?,
            bytes: 0,
            claimed_until: None,
            replied: r.get(4)?,
            reply: r.get(5)?,
            reply_at: r.get(6)?,
            collected_at: Some(r.get(2)?),
        })
    })?;
    all.extend(collected.collect::<rusqlite::Result<Vec<_>>>()?);
    all.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(all)
}

/// Claim a report and hand back its contents.
///
/// Claiming and reading are one step because they are one intent, and splitting
/// them would let a second administrator read something the first is already
/// dealing with.
pub fn claim(conn: &Connection, id: i64, now: i64) -> Result<String, PostboxError> {
    let (body, claimed_until): (String, Option<i64>) = conn
        .query_row(
            "SELECT body, claimed_until FROM diagnoses WHERE id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|_| PostboxError::NotFound)?;

    // A live lease does NOT refuse the holder of the administrator token, because
    // there is only one such holder: the token is the identity. Refusing it
    // meant that a download interrupted halfway left the report unreadable for
    // fifteen minutes - the very situation the lease exists to survive.
    //
    // What the lease still does is keep `release` honest: nothing is deleted
    // that was not first claimed and read.
    if claimed_until.map(|u| u > now).unwrap_or(false) {
        info!("diagnosis {} was already claimed; lease renewed", id);
    }
    conn.execute(
        "UPDATE diagnoses SET claimed_until = ?2 WHERE id = ?1",
        params![id, now + CLAIM_LEASE_SECS],
    )
    .map_err(|_| PostboxError::NotFound)?;
    info!("diagnosis {} claimed for {} s", id, CLAIM_LEASE_SECS);
    Ok(body)
}

/// Say what happened to a claimed report, and let it go.
///
/// This is the only thing that removes one, which is what makes a failed
/// download harmless: nothing was lost, the lease simply runs out and the report
/// is free again.
pub fn release(conn: &Connection, id: i64, now: i64) -> Result<(), PostboxError> {
    let claimed: Option<i64> = conn
        .query_row("SELECT claimed_until FROM diagnoses WHERE id = ?1", params![id], |r| r.get(0))
        .map_err(|_| PostboxError::NotFound)?;
    if !claimed.map(|u| u > now).unwrap_or(false) {
        return Err(PostboxError::NotClaimed);
    }
    let station: Option<i64> = conn
        .query_row("SELECT station_id FROM diagnoses WHERE id = ?1", params![id], |r| r.get(0))
        .unwrap_or(None);

    // Marker first, delete second, both or neither.
    //
    // The other order lost the marker's failure: the report was deleted and the
    // insert that follows had its error thrown away, so on a full disk or an
    // I/O fault the report was gone AND unanswerable - `reply` looks in exactly
    // these two places and would find neither. Silently, at the moment the
    // administrator was collecting it to help somebody. Found in review
    // (2026-08-18); it matters more since the marker also decides whether a
    // collected report is still listed at all.
    let tx = conn.unchecked_transaction().map_err(|_| PostboxError::NotFound)?;
    if let Some(station) = station {
        // A marker and not the report: which station, and when. Enough to
        // answer afterwards, nothing to read.
        tx.execute(
            "INSERT OR REPLACE INTO collected (diagnosis_id, station_id, at) VALUES (?1,?2,?3)",
            params![id, station, now],
        )
        .map_err(|e| {
            warn!("diagnosis {}: could not record the collection ({}) - not removing it", id, e);
            PostboxError::NotFound
        })?;
    }
    tx.execute("DELETE FROM diagnoses WHERE id = ?1", params![id])
        .map_err(|_| PostboxError::NotFound)?;
    tx.commit().map_err(|e| {
        warn!("diagnosis {}: collection could not be committed ({}) - it stays in the postbox", id, e);
        PostboxError::NotFound
    })?;
    info!("diagnosis {} released and removed from the postbox", id);
    Ok(())
}

/// One short answer back to whoever sent a report (design §1.5).
pub fn reply(conn: &Connection, id: i64, text: &str, now: i64) -> Result<i64, PostboxError> {
    // The station, from the report if it is still here and from the marker if it
    // has been collected. Answering after collecting is the ORDINARY case - the
    // documented way of working fetches and releases in one step - and it used
    // to fail with "not found" and no reason given.
    let station: i64 = conn
        .query_row(
            "SELECT station_id FROM diagnoses WHERE id = ?1 AND station_id IS NOT NULL
             UNION ALL
             SELECT station_id FROM collected WHERE diagnosis_id = ?1
             LIMIT 1",
            params![id],
            |r| r.get(0),
        )
        .map_err(|_| PostboxError::NotFound)?;

    // At most one, so this stays an answer and does not become a conversation.
    let existing: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM replies WHERE diagnosis_id = ?1",
            params![id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if existing > 0 {
        return Err(PostboxError::AlreadyReplied);
    }

    conn.execute(
        "INSERT INTO replies (station_id, diagnosis_id, body, at) VALUES (?1,?2,?3,?4)",
        params![station, id, text, now],
    )
    .map_err(|_| PostboxError::NotFound)?;
    info!("diagnosis {}: answered, waiting for station {} to fetch it", id, station);
    Ok(station)
}


/// Replies waiting for one station.
pub fn replies_for(
    conn: &Connection,
    station_id: i64,
    now: i64,
) -> rusqlite::Result<Vec<(i64, String, i64)>> {
    let mut q = conn.prepare(
        "SELECT id, body, at FROM replies WHERE station_id = ?1 ORDER BY id ASC",
    )?;
    let rows = q
        .query_map(params![station_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<Vec<(i64, String, i64)>>>()?;

    // Fetching is delivery. It is the only moment this service can observe, and
    // without it an answer nobody ever collected would be pruned on the same
    // clock as one that arrived - and logged with the same word.
    if !rows.is_empty() {
        let _ = conn.execute(
            "UPDATE replies SET delivered_at = ?2 WHERE station_id = ?1 AND delivered_at IS NULL",
            params![station_id, now],
        );
    }
    Ok(rows)
}


/// An uncollected report of somebody who just left the chat goes with them
/// (design §6.4). One already collected lives on the administrator's own
/// machine and is beyond the reach of anything here, which the consent text
/// says in as many words.
pub fn forget_station(conn: &Connection, station_id: i64) -> rusqlite::Result<usize> {
    let n = conn.execute("DELETE FROM diagnoses WHERE station_id = ?1", params![station_id])?;
    if n > 0 {
        info!("station {}: {} uncollected diagnosis report(s) removed", station_id, n);
    }
    // Their answers and markers go with them (design section 6.4). An answer to
    // a report that no longer exists, for a station that no longer exists, is
    // exactly the kind of row nobody thinks to look for.
    let _ = conn.execute("DELETE FROM replies WHERE station_id = ?1", params![station_id]);
    let _ = conn.execute("DELETE FROM collected WHERE station_id = ?1", params![station_id]);
    Ok(n)
}

/// What housekeeping removed, or would remove.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub reports: usize,
    pub delivered_replies: usize,
    pub undelivered_replies: usize,
    pub markers: usize,
}

/// What housekeeping would remove at `at`, without removing anything.
///
/// This runs the real thing inside a transaction and rolls it back, rather
/// than asking the same questions with a second set of `SELECT COUNT`s. Two
/// sets of conditions for one decision drift apart the moment one is edited,
/// which is a fault this project has paid for more than once - and here they
/// could not be kept identical anyway: the markers are removed last, on
/// purpose, and only once the answers that point at them are already gone. A
/// count taken before any of that would be right about a database that never
/// existed.
///
/// It exists because the shortest of these periods is thirty days. Nothing can
/// be observed by waiting, and the deletions themselves are the one thing a
/// test on live data must not do.
pub fn preview(conn: &Connection, at: i64) -> rusqlite::Result<Counts> {
    let tx = conn.unchecked_transaction()?;
    let counts = expire(&tx, at)?;
    tx.rollback()?;
    Ok(counts)
}

/// Housekeeping: what nobody came for.
pub fn prune(conn: &Connection, now: i64) -> rusqlite::Result<Counts> {
    let counts = expire(conn, now)?;
    if counts.reports > 0 {
        info!(
            "{} uncollected diagnosis report(s) expired after {} days",
            counts.reports, UNCOLLECTED_MAX_DAYS
        );
    }
    if counts.delivered_replies > 0 {
        info!(
            "{} delivered answer(s) removed after {} days",
            counts.delivered_replies, DELIVERED_REPLY_MAX_DAYS
        );
    }
    if counts.undelivered_replies > 0 {
        // Worth a warning and not a note: somebody was answered and never came
        // back for it, which is a fault report that ends in silence.
        warn!(
            "{} answer(s) expired after {} days without ever being fetched",
            counts.undelivered_replies, UNCOLLECTED_MAX_DAYS
        );
    }
    if counts.markers > 0 {
        info!("{} collected-report marker(s) expired", counts.markers);
    }
    Ok(counts)
}

/// The deletions themselves, without a word. Shared by the real run and the
/// preview so there is exactly one place where the periods are decided.
fn expire(conn: &Connection, now: i64) -> rusqlite::Result<Counts> {
    let cutoff = now - UNCOLLECTED_MAX_DAYS * 86_400;
    let n = conn.execute("DELETE FROM diagnoses WHERE at < ?1", params![cutoff])?;

    // Answers age on their own clock, and on which of two clocks depends on
    // whether they ever arrived. Pruning both on the report's age said
    // "uncollected" about an answer that had been written and read, and swept up
    // one that had never been fetched with the same word.
    let delivered = conn.execute(
        "DELETE FROM replies WHERE delivered_at IS NOT NULL AND delivered_at < ?1",
        params![now - DELIVERED_REPLY_MAX_DAYS * 86_400],
    )?;
    let undelivered = conn.execute(
        "DELETE FROM replies WHERE delivered_at IS NULL AND at < ?1",
        params![cutoff],
    )?;

    // The markers go last and only when nothing points at them any more.
    let marks = conn.execute(
        "DELETE FROM collected WHERE at < ?1
         AND diagnosis_id NOT IN (SELECT diagnosis_id FROM replies)",
        params![cutoff],
    )?;
    Ok(Counts {
        reports: n,
        delivered_replies: delivered,
        undelivered_replies: undelivered,
        markers: marks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: i64 = 1_700_000_000;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        init(&c).unwrap();
        c
    }

    #[test]
    fn a_report_goes_in_and_shows_up_waiting() {
        let c = db();
        let id = submit(&c, 7, Some("PA0ABC"), "logregel", T).unwrap();
        let waiting = list(&c, T).unwrap();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].id, id);
        assert_eq!(waiting[0].claimed_until, None, "nobody has it yet");
    }

    #[test]
    fn an_oversized_report_is_refused() {
        let c = db();
        let big = "x".repeat(MAX_REPORT_BYTES + 1);
        assert_eq!(submit(&c, 7, Some("X"), &big, T), Err(PostboxError::TooLarge));
    }

    #[test]
    fn a_station_cannot_fill_the_postbox_on_its_own() {
        let c = db();
        for i in 0..MAX_REPORTS_PER_STATION_PER_DAY {
            submit(&c, 7, Some("X"), &format!("r{i}"), T).unwrap();
        }
        assert_eq!(submit(&c, 7, Some("X"), "nog een", T), Err(PostboxError::TooMany));
        // Tomorrow is a new day.
        assert!(submit(&c, 7, Some("X"), "morgen", T + 86_401).is_ok());
        // And another station is unaffected.
        assert!(submit(&c, 8, Some("Y"), "van iemand anders", T).is_ok());
    }

    /// The whole reason for two phases: a claim alone must not lose anything.
    #[test]
    fn claiming_does_not_remove_it() {
        let c = db();
        let id = submit(&c, 7, Some("X"), "inhoud", T).unwrap();
        assert_eq!(claim(&c, id, T).unwrap(), "inhoud");
        assert_eq!(list(&c, T).unwrap().len(), 1, "still there after claiming");
    }

    /// Re-reading one's own claim has to work: there is a single administrator
    /// token, so a live lease is always this reader's own, and a download that
    /// broke halfway is precisely when it must be possible to try again.
    #[test]
    fn claiming_again_renews_the_lease_and_still_reads() {
        let c = db();
        let id = submit(&c, 7, Some("X"), "inhoud", T).unwrap();
        claim(&c, id, T).unwrap();
        assert_eq!(claim(&c, id, T + 10).unwrap(), "inhoud");
        let until = list(&c, T + 10).unwrap()[0].claimed_until.unwrap();
        assert_eq!(until, T + 10 + CLAIM_LEASE_SECS, "the lease moved with it");
    }

    /// The lease still guards the one thing that matters: nothing is deleted
    /// that was not first claimed and read.
    #[test]
    fn an_unclaimed_report_cannot_be_released() {
        let c = db();
        let id = submit(&c, 7, Some("X"), "inhoud", T).unwrap();
        assert_eq!(release(&c, id, T), Err(PostboxError::NotClaimed));
        assert_eq!(list(&c, T).unwrap().len(), 1, "still there");
    }

    /// A browser closed halfway must not strand a report forever.
    #[test]
    fn an_expired_claim_falls_back_to_free() {
        let c = db();
        let id = submit(&c, 7, Some("X"), "inhoud", T).unwrap();
        claim(&c, id, T).unwrap();
        let later = T + CLAIM_LEASE_SECS + 1;
        assert_eq!(list(&c, later).unwrap()[0].claimed_until, None);
        assert!(claim(&c, id, later).is_ok(), "somebody else may pick it up");
    }

    #[test]
    fn releasing_removes_it_and_only_after_a_claim() {
        let c = db();
        let id = submit(&c, 7, Some("X"), "inhoud", T).unwrap();
        assert_eq!(release(&c, id, T), Err(PostboxError::NotClaimed), "no claim, no removal");
        claim(&c, id, T).unwrap();
        assert!(release(&c, id, T).is_ok());

        // The body is gone - that is what collecting means, and it is how the
        // VPS stays empty of other people's logs. The row does not disappear
        // from the listing though: answering a collected report is the ordinary
        // way of working, and while it vanished the only route that could still
        // do it was the command line, which is not where the person answering
        // is (2026-08-17).
        let after = list(&c, T).unwrap();
        assert_eq!(after.len(), 1, "still listed, so it can still be answered");
        assert!(after[0].collected_at.is_some(), "and marked as collected");
        assert_eq!(after[0].bytes, 0, "with nothing left to read here");
        assert!(claim(&c, id, T).is_err(), "and nothing to claim either");
    }

    /// The listing must not lose a collected report behind a still-held one, or
    /// show it twice while both exist. Both halves come from different tables
    /// and are merged here, which is exactly where an off-by-one hides.
    #[test]
    fn collected_and_waiting_reports_appear_once_each_in_order() {
        let c = db();
        let a = submit(&c, 7, Some("X"), "eerste", T).unwrap();
        let b = submit(&c, 7, Some("X"), "tweede", T).unwrap();
        claim(&c, a, T).unwrap();
        release(&c, a, T).unwrap();

        let got = list(&c, T).unwrap();
        assert_eq!(got.len(), 2, "one of each, no duplicates");
        assert_eq!(got[0].id, b, "newest first, whichever table it came from");
        assert_eq!(got[1].id, a);
        assert!(got[0].collected_at.is_none(), "the one still held");
        assert!(got[1].collected_at.is_some(), "the one on the administrator's PC");
    }

    /// The blocker from the release review: collecting used to delete the report
    /// and then write the marker with its error thrown away. On a full disk that
    /// left the report gone AND unanswerable - `reply` looks in the report table
    /// or the marker table and would find neither - silently, at the moment
    /// somebody was collecting it in order to help.
    ///
    /// Forced here by taking the marker table away, which is the same shape as
    /// any write failure: nothing may be removed unless the marker survives.
    #[test]
    fn a_collection_that_cannot_be_recorded_removes_nothing() {
        let c = db();
        let id = submit(&c, 7, Some("X"), "inhoud", T).unwrap();
        claim(&c, id, T).unwrap();
        c.execute_batch("DROP TABLE collected").unwrap();

        assert!(release(&c, id, T).is_err(), "no marker, no removal");

        // Still there, still readable, still answerable.
        c.execute_batch(
            "CREATE TABLE IF NOT EXISTS collected (
                diagnosis_id INTEGER PRIMARY KEY,
                station_id   INTEGER NOT NULL,
                at           INTEGER NOT NULL);",
        )
        .unwrap();
        let after = list(&c, T).unwrap();
        assert_eq!(after.len(), 1, "the report stayed in the postbox");
        assert!(after[0].collected_at.is_none(), "and is not marked collected");
        assert!(reply(&c, id, "alsnog een antwoord", T).is_ok(), "and can still be answered");
    }

    /// A marker and a live report for the same number: one row, and the live
    /// one wins.
    ///
    /// It should not happen - `release` writes the marker and deletes the
    /// report in one transaction - but "should not" is exactly what a listing
    /// built from two tables has to survive. The live side wins because its
    /// body is still here and still claimable; showing the marker instead would
    /// hide a report that can be read (raised in review, 2026-08-18).
    #[test]
    fn a_marker_beside_a_live_report_shows_one_row_and_it_is_the_live_one() {
        let c = db();
        let id = submit(&c, 7, Some("X"), "inhoud", T).unwrap();
        // The state that should be impossible, made by hand.
        c.execute(
            "INSERT INTO collected (diagnosis_id, station_id, at) VALUES (?1,?2,?3)",
            params![id, 7, T],
        )
        .unwrap();

        let got = list(&c, T).unwrap();
        assert_eq!(got.len(), 1, "one report, one row");
        assert_eq!(got[0].id, id);
        assert!(got[0].collected_at.is_none(), "the live row wins");
        assert!(got[0].bytes > 0, "and it still has a body to read");
    }

    /// Clicking twice must not half-remove anything or report a phantom failure.
    #[test]
    fn releasing_twice_is_harmless() {
        let c = db();
        let id = submit(&c, 7, Some("X"), "inhoud", T).unwrap();
        claim(&c, id, T).unwrap();
        release(&c, id, T).unwrap();
        assert_eq!(release(&c, id, T), Err(PostboxError::NotFound));
    }

    /// The collision the review found: collecting a report deletes it, and the
    /// answer used to live in that same row - so it went out with it and the
    /// sender waited for something that had been written and thrown away.
    #[test]
    fn an_answer_survives_the_report_being_collected() {
        let c = db();
        let id = submit(&c, 7, Some("X"), "inhoud", T).unwrap();
        reply(&c, id, "kijk eens naar je audio-apparaat", T).unwrap();
        claim(&c, id, T).unwrap();
        release(&c, id, T).unwrap();
        let after = list(&c, T).unwrap();
        assert_eq!(after.len(), 1, "the marker stays so it can still be answered");
        assert!(after[0].collected_at.is_some());
        assert_eq!(after[0].replied, true, "and the listing knows it was answered");
        let got = replies_for(&c, 7, T).unwrap();
        assert_eq!(got.len(), 1, "and the answer survived the collection");
    }

    /// The documented way of working fetches and releases in one step, so
    /// answering afterwards is the ordinary case - and used to fail with
    /// "not found" and no reason given.
    #[test]
    fn answering_still_works_after_the_report_was_collected() {
        let c = db();
        let id = submit(&c, 7, Some("X"), "inhoud", T).unwrap();
        claim(&c, id, T).unwrap();
        release(&c, id, T).unwrap();
        assert_eq!(reply(&c, id, "alsnog een antwoord", T).unwrap(), 7);
        assert_eq!(replies_for(&c, 7, T).unwrap().len(), 1);
    }

    /// Fetching is delivery, and delivered is not the same as expired: they age
    /// on different clocks and are logged with different words.
    #[test]
    fn a_delivered_answer_and_one_nobody_fetched_age_differently() {
        let c = db();
        let a = submit(&c, 7, Some("X"), "een", T).unwrap();
        let b = submit(&c, 8, Some("Y"), "twee", T).unwrap();
        reply(&c, a, "opgehaald", T).unwrap();
        reply(&c, b, "nooit opgehaald", T).unwrap();
        replies_for(&c, 7, T).unwrap(); // station 7 collects theirs

        // A week and a bit later: the delivered one goes, the other stays.
        prune(&c, T + (DELIVERED_REPLY_MAX_DAYS + 1) * 86_400).unwrap();
        assert!(replies_for(&c, 7, T).unwrap().is_empty(), "delivered, and done");
        assert_eq!(replies_for(&c, 8, T).unwrap().len(), 1, "still waiting");
    }

    /// Leaving the chat takes the answers with it, or a row survives pointing at
    /// a station that no longer exists.
    #[test]
    fn leaving_takes_the_answers_too() {
        let c = db();
        let id = submit(&c, 7, Some("X"), "inhoud", T).unwrap();
        reply(&c, id, "antwoord", T).unwrap();
        forget_station(&c, 7).unwrap();
        assert!(replies_for(&c, 7, T).unwrap().is_empty());
    }

    #[test]
    fn one_reply_per_report_and_the_sender_can_find_it() {
        let c = db();
        let id = submit(&c, 7, Some("X"), "inhoud", T).unwrap();
        assert_eq!(reply(&c, id, "kijk eens naar je audio-apparaat", T).unwrap(), 7);
        // At most one, so this stays an answer rather than a conversation.
        assert!(reply(&c, id, "en nog iets", T).is_err());
        let got = replies_for(&c, 7, T).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, "kijk eens naar je audio-apparaat");
    }

    /// Leaving the chat takes an uncollected report with it (§6.4).
    #[test]
    fn leaving_the_chat_removes_an_uncollected_report() {
        let c = db();
        submit(&c, 7, Some("X"), "inhoud", T).unwrap();
        submit(&c, 8, Some("Y"), "van een ander", T).unwrap();
        assert_eq!(forget_station(&c, 7).unwrap(), 1);
        let left = list(&c, T).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].station_id, Some(8));
    }

    #[test]
    fn what_nobody_came_for_expires() {
        let c = db();
        submit(&c, 7, Some("X"), "oud", T - (UNCOLLECTED_MAX_DAYS + 1) * 86_400).unwrap();
        submit(&c, 7, Some("X"), "vers", T).unwrap();
        assert_eq!(prune(&c, T).unwrap().reports, 1);
        assert_eq!(list(&c, T).unwrap().len(), 1);
    }

    /// The guarantee the preview rests on: what it reports is what a real run
    /// then does. Anything that changes one and not the other fails here
    /// rather than in a month, on live data, in the one direction that cannot
    /// be undone.
    #[test]
    fn the_preview_is_what_pruning_then_removes() {
        let c = db();
        let old = T - (UNCOLLECTED_MAX_DAYS + 1) * 86_400;
        submit(&c, 7, Some("X"), "oud", old).unwrap();
        submit(&c, 8, Some("Y"), "ouder", old - 86_400).unwrap();
        submit(&c, 9, Some("Z"), "vers", T).unwrap();

        let foreseen = preview(&c, T).unwrap();
        let done = prune(&c, T).unwrap();
        assert_eq!(foreseen, done, "the preview promised something else");
        assert_eq!(done.reports, 2);
        assert_eq!(list(&c, T).unwrap().len(), 1);
    }

    /// And it must leave the postbox exactly as it found it.
    #[test]
    fn a_preview_removes_nothing() {
        let c = db();
        submit(&c, 7, Some("X"), "oud", T - (UNCOLLECTED_MAX_DAYS + 1) * 86_400).unwrap();
        let before = list(&c, T).unwrap().len();
        preview(&c, T).unwrap();
        // Twice, because a rollback that only works once is worse than none.
        preview(&c, T).unwrap();
        assert_eq!(list(&c, T).unwrap().len(), before);
    }

    /// Asking about a date in the future is the whole point: nothing here is
    /// thirty days old yet, and nobody can wait for it.
    #[test]
    fn it_answers_for_a_date_that_has_not_arrived() {
        let c = db();
        submit(&c, 7, Some("X"), "vandaag", T).unwrap();
        assert_eq!(preview(&c, T).unwrap().reports, 0);
        let later = T + (UNCOLLECTED_MAX_DAYS + 1) * 86_400;
        assert_eq!(preview(&c, later).unwrap().reports, 1);
        // Still there: it was a question, not an instruction.
        assert_eq!(list(&c, T).unwrap().len(), 1);
    }
}
