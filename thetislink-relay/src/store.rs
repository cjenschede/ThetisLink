// SPDX-License-Identifier: GPL-2.0-or-later

//! Fase 1 user-auth: per-station key registry backed by SQLite.
//!
//! Each station is identified by a high-entropy secret; the station **name** is
//! only a label. The relay validates a presented secret against this registry and
//! routes traffic by the returned stable row id (so two stations may share a name
//! as long as their secrets differ). Only the secret **hash** is stored - never the
//! plaintext. Secrets are machine-generated 256-bit values, so a fast hash
//! (SHA-256) is sufficient for lookup; a slow KDF is only needed for human
//! passwords (that arrives with the Fase 2 dashboard login).
//!
//! The schema already reserves the Fase 2 quota columns so the dashboard can build
//! on the same table without a migration.

// Fase 2 dashboard store API (devices/admin/rotate) is added ahead of the admin API
// that consumes it (next increment); allow dead_code until it is wired in.
#![allow(dead_code)]

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};

/// A station registry row (secret hash omitted - never surfaced).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StationRow {
    pub id: i64,
    pub label: String,
    pub owner: String,
    pub enabled: bool,
    pub created_at: i64,
}

/// A device record for the dashboard (no secrets). install_id is the stable key;
/// name is a human label; the rest is persistent analytics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceRow {
    pub id: i64,
    pub station_id: i64,
    pub install_id: String,
    pub enroll_seq: i64,
    pub platform: String,
    pub name: Option<String>,
    pub enabled: bool,
    pub first_seen: i64,
    pub last_seen: i64,
    pub sessions: i64,
    pub bytes_total: i64,
    pub last_ip: Option<String>,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating if needed) the station registry at `path`. Use `":memory:"`
    /// for tests. WAL mode so the Fase 2 dashboard can write while the relay reads.
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening station registry {path}"))?;
        // WAL + busy_timeout: the Fase 2 dashboard writes while the relay reads
        // (security note). No-op / harmless on an in-memory db.
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS stations (
                id           INTEGER PRIMARY KEY,
                label        TEXT    NOT NULL,
                secret_hash  TEXT    NOT NULL UNIQUE,
                owner        TEXT    NOT NULL DEFAULT '',
                enabled      INTEGER NOT NULL DEFAULT 1,
                created_at   INTEGER NOT NULL DEFAULT 0,
                -- Fase 2 quota columns (reserved; unused in Fase 1).
                max_clients  INTEGER,
                max_kbps     INTEGER
            );
            -- Fase 2: per-device records (dashboard schema). install_id = stable key; name
            -- is a label. Persistent analytics: last_seen / sessions / bytes_total.
            CREATE TABLE IF NOT EXISTS devices (
                id           INTEGER PRIMARY KEY,
                station_id   INTEGER NOT NULL,
                install_id   TEXT    NOT NULL,
                enroll_seq   INTEGER NOT NULL DEFAULT 0,
                platform     TEXT    NOT NULL DEFAULT '',
                name         TEXT,
                enabled      INTEGER NOT NULL DEFAULT 1,
                -- legacy, unused: there is no per-device approval gate. Admission rests on
                -- the station secret + max_devices + the `enabled` blocklist. Kept as a
                -- dormant column so old and new DBs share one schema.
                approved     INTEGER NOT NULL DEFAULT 0,
                first_seen   INTEGER NOT NULL DEFAULT 0,
                last_seen    INTEGER NOT NULL DEFAULT 0,
                sessions     INTEGER NOT NULL DEFAULT 0,
                bytes_total  INTEGER NOT NULL DEFAULT 0,
                last_ip      TEXT,
                UNIQUE(station_id, install_id)
            );
            -- Fase 2: single admin account for the dashboard (id is pinned to 1).
            -- Human password -> Argon2id hash (never SHA-256, never plaintext).
            CREATE TABLE IF NOT EXISTS admin (
                id            INTEGER PRIMARY KEY CHECK (id = 1),
                password_hash TEXT    NOT NULL,
                created_at    INTEGER NOT NULL DEFAULT 0
            );
            -- Fase 3: per-station monthly usage buckets ('YYYY-MM'). One row per
            -- station per month; a new month is simply a new row (no reset job).
            CREATE TABLE IF NOT EXISTS station_usage (
                station_id INTEGER NOT NULL,
                ym         TEXT    NOT NULL,
                bytes      INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (station_id, ym)
            );",
        )
        .context("creating tables")?;
        // Additive migrations for DBs created before Fase 3 (ignore 'duplicate column'
        // on a fresh DB where the column may already exist). max_devices = cap on
        // enrolled (enabled) devices; max_monthly_bytes = monthly data cap (NULL = no limit).
        for stmt in [
            "ALTER TABLE stations ADD COLUMN max_devices INTEGER",
            "ALTER TABLE stations ADD COLUMN max_monthly_bytes INTEGER",
        ] {
            let _ = conn.execute(stmt, []);
        }
        Ok(Self { conn })
    }

    /// Number of registered stations (enabled or not). Drives the auth-mode
    /// decision: >0 stations -> per-station registry auth, else legacy token.
    pub fn count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM stations", [], |r| r.get(0))?)
    }

    /// Total number of device records across all stations (dashboard stats).
    pub fn device_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM devices", [], |r| r.get(0))?)
    }

    /// Validate a presented secret. Returns the stable station id when a matching,
    /// **enabled** station exists; `None` otherwise. Lookup is by secret hash - the
    /// plaintext is never stored or compared.
    pub fn authenticate(&self, secret: &str) -> Result<Option<i64>> {
        let hash = hash_secret(secret);
        let row = self
            .conn
            .query_row(
                "SELECT id, enabled FROM stations WHERE secret_hash = ?1",
                [hash],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()
            .context("station lookup")?;
        Ok(match row {
            Some((id, enabled)) if enabled != 0 => Some(id),
            _ => None,
        })
    }

    /// Register a new station with the given secret. Returns the new row id.
    pub fn add(&self, label: &str, owner: &str, secret: &str, created_at: i64) -> Result<i64> {
        let hash = hash_secret(secret);
        self.conn
            .execute(
                "INSERT INTO stations (label, secret_hash, owner, enabled, created_at)
                 VALUES (?1, ?2, ?3, 1, ?4)",
                rusqlite::params![label, hash, owner, created_at],
            )
            .context("inserting station")?;
        Ok(self.conn.last_insert_rowid())
    }

    /// All stations, oldest first (secret hashes never returned).
    pub fn list(&self) -> Result<Vec<StationRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, label, owner, enabled, created_at FROM stations ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(StationRow {
                    id: r.get(0)?,
                    label: r.get(1)?,
                    owner: r.get(2)?,
                    enabled: r.get::<_, i64>(3)? != 0,
                    created_at: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Enable/disable a station. Returns true if a row was affected.
    pub fn set_enabled(&self, id: i64, enabled: bool) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE stations SET enabled = ?2 WHERE id = ?1",
            rusqlite::params![id, enabled as i64],
        )?;
        Ok(n > 0)
    }

    /// Delete a station. Returns true if a row was removed.
    pub fn remove(&self, id: i64) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM stations WHERE id = ?1", [id])?;
        Ok(n > 0)
    }

    // --- Station mutations for the dashboard ---

    /// Replace a station's secret (rotate). Old secret stops working immediately.
    pub fn rotate_secret(&self, id: i64, new_secret: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE stations SET secret_hash = ?2 WHERE id = ?1",
            rusqlite::params![id, hash_secret(new_secret)],
        )?;
        Ok(n > 0)
    }

    /// Rename a station's label (does not affect its secret/identity).
    pub fn set_label(&self, id: i64, label: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE stations SET label = ?2 WHERE id = ?1",
            rusqlite::params![id, label],
        )?;
        Ok(n > 0)
    }

    // --- Admin account (Fase 2 dashboard login) ---

    pub fn has_admin(&self) -> Result<bool> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM admin", [], |r| r.get(0))?;
        Ok(n > 0)
    }

    /// Set (or replace) the single admin password hash (Argon2id string).
    pub fn set_admin_password_hash(&self, hash: &str, now: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO admin (id, password_hash, created_at) VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET password_hash = ?1",
            rusqlite::params![hash, now],
        )?;
        Ok(())
    }

    pub fn admin_password_hash(&self) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT password_hash FROM admin WHERE id = 1", [], |r| {
                r.get(0)
            })
            .optional()?)
    }

    // --- Devices ---

    /// Upsert device presence on connect: on first sight insert with the next
    /// per-(station,platform) enroll_seq so it shows in the dashboard; otherwise refresh
    /// last_seen/name/last_ip. Returns `(id, enabled)` so the caller can enforce the
    /// `enabled` blocklist. Does NOT bump the session count - that is `bump_session`,
    /// called only once a device is actually admitted. Called once per connect (a
    /// lifecycle edge), never per frame.
    pub fn enroll_seen(
        &self,
        station_id: i64,
        install_id: &str,
        name: Option<&str>,
        platform: &str,
        ip: Option<&str>,
        now: i64,
    ) -> Result<(i64, bool)> {
        self.conn.execute(
            "INSERT INTO devices
                 (station_id, install_id, enroll_seq, platform, name, first_seen, last_seen, sessions, last_ip)
             VALUES (?1, ?2,
                 (SELECT COALESCE(MAX(enroll_seq),0)+1 FROM devices WHERE station_id=?1 AND platform=?4),
                 ?4, ?3, ?5, ?5, 0, ?6)
             ON CONFLICT(station_id, install_id) DO UPDATE SET
                 last_seen = ?5,
                 name      = COALESCE(?3, name),
                 last_ip   = ?6",
            rusqlite::params![station_id, install_id, name, platform, now, ip],
        )?;
        Ok(self.conn.query_row(
            "SELECT id, enabled FROM devices WHERE station_id=?1 AND install_id=?2",
            rusqlite::params![station_id, install_id],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? != 0)),
        )?)
    }

    /// Count one admitted session (device was admitted and registered). Separate from
    /// `enroll_seen` so refused connect attempts do not inflate the count.
    pub fn bump_session(&self, id: i64, now: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE devices SET sessions = sessions + 1, last_seen = ?2 WHERE id = ?1",
            rusqlite::params![id, now],
        )?;
        Ok(())
    }

    /// `(station_id, install_id, enabled)` for one device - lets the admin API decide
    /// whether a just-blocked device must be kicked off the relay now.
    pub fn device_admit_info(&self, id: i64) -> Result<Option<(i64, String, bool)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT station_id, install_id, enabled FROM devices WHERE id = ?1",
                [id],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)? != 0,
                    ))
                },
            )
            .optional()?)
    }

    /// All devices of a station, in enrollment order (no secrets).
    pub fn list_devices(&self, station_id: i64) -> Result<Vec<DeviceRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, station_id, install_id, enroll_seq, platform, name, enabled,
                    first_seen, last_seen, sessions, bytes_total, last_ip
             FROM devices WHERE station_id = ?1 ORDER BY enroll_seq",
        )?;
        let rows = stmt
            .query_map([station_id], |r| {
                Ok(DeviceRow {
                    id: r.get(0)?,
                    station_id: r.get(1)?,
                    install_id: r.get(2)?,
                    enroll_seq: r.get(3)?,
                    platform: r.get(4)?,
                    name: r.get(5)?,
                    enabled: r.get::<_, i64>(6)? != 0,
                    first_seen: r.get(7)?,
                    last_seen: r.get(8)?,
                    sessions: r.get(9)?,
                    bytes_total: r.get(10)?,
                    last_ip: r.get(11)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn rename_device(&self, id: i64, name: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE devices SET name = ?2 WHERE id = ?1",
            rusqlite::params![id, name],
        )?;
        Ok(n > 0)
    }

    pub fn set_device_enabled(&self, id: i64, enabled: bool) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE devices SET enabled = ?2 WHERE id = ?1",
            rusqlite::params![id, enabled as i64],
        )?;
        Ok(n > 0)
    }

    pub fn remove_device(&self, id: i64) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM devices WHERE id = ?1", [id])?;
        Ok(n > 0)
    }

    /// Add to a device's cumulative byte total. Called on disconnect with the
    /// session total accumulated in a lock-free atomic - never a per-frame DB
    /// write (hot-path write guard).
    pub fn add_device_bytes(&self, id: i64, delta: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE devices SET bytes_total = bytes_total + ?2 WHERE id = ?1",
            rusqlite::params![id, delta],
        )?;
        Ok(())
    }

    /// Add to a device's cumulative byte total addressed by (station, install_id)
    /// instead of row id - used by the periodic mid-session flush, which only holds
    /// the peer's install id. No-op if the device row is gone.
    pub fn add_device_bytes_by_install(
        &self,
        station_id: i64,
        install_id: &str,
        delta: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE devices SET bytes_total = bytes_total + ?3
             WHERE station_id = ?1 AND install_id = ?2",
            rusqlite::params![station_id, install_id, delta],
        )?;
        Ok(())
    }

    // --- Fase 3: per-station monthly usage (data caps + analytics) ---

    /// Add a session's bytes to a station's monthly bucket (`ym` = "YYYY-MM").
    /// Called once on disconnect alongside `add_device_bytes` - never per frame.
    pub fn add_station_month_bytes(&self, station_id: i64, ym: &str, delta: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO station_usage (station_id, ym, bytes) VALUES (?1, ?2, ?3)
             ON CONFLICT(station_id, ym) DO UPDATE SET bytes = bytes + ?3",
            rusqlite::params![station_id, ym, delta],
        )?;
        Ok(())
    }

    /// A station's used bytes in month `ym` (0 if none recorded yet).
    pub fn station_month_bytes(&self, station_id: i64, ym: &str) -> Result<i64> {
        Ok(self
            .conn
            .query_row(
                "SELECT bytes FROM station_usage WHERE station_id = ?1 AND ym = ?2",
                rusqlite::params![station_id, ym],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    /// `(max_devices, max_clients, max_monthly_bytes)` for a station; each `None`
    /// means "no limit". Drives the connect-time admission caps.
    pub fn station_limits(&self, id: i64) -> Result<(Option<i64>, Option<i64>, Option<i64>)> {
        Ok(self
            .conn
            .query_row(
                "SELECT max_devices, max_clients, max_monthly_bytes FROM stations WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .unwrap_or((None, None, None)))
    }

    /// Set a station's limits (each `None` clears that limit). Used by the dashboard.
    pub fn set_station_limits(
        &self,
        id: i64,
        max_devices: Option<i64>,
        max_clients: Option<i64>,
        max_monthly_bytes: Option<i64>,
    ) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE stations SET max_devices = ?2, max_clients = ?3, max_monthly_bytes = ?4
             WHERE id = ?1",
            rusqlite::params![id, max_devices, max_clients, max_monthly_bytes],
        )?;
        Ok(n > 0)
    }

    /// Number of enabled (not-blocked) devices for a station. This is what the
    /// max_devices cap counts: how many distinct devices may use the station.
    pub fn count_enabled_devices(&self, station_id: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM devices WHERE station_id = ?1 AND enabled = 1",
            [station_id],
            |r| r.get(0),
        )?)
    }

    /// `Some(enabled)` if the device already exists for this station, else `None`
    /// (a brand-new device). Lets the connect gate apply the max_devices cap only to
    /// genuinely new devices and refuse a blocked one.
    pub fn device_enabled(&self, station_id: i64, install_id: &str) -> Result<Option<bool>> {
        Ok(self
            .conn
            .query_row(
                "SELECT enabled FROM devices WHERE station_id = ?1 AND install_id = ?2",
                rusqlite::params![station_id, install_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .map(|e| e != 0))
    }

    /// A station's monthly usage history, most recent month first (for analytics).
    pub fn station_month_history(&self, station_id: i64, limit: i64) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT ym, bytes FROM station_usage WHERE station_id = ?1 ORDER BY ym DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![station_id, limit], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Consistent whole-database snapshot as one file's bytes, for the admin backup
    /// download. `VACUUM INTO` produces a clean, self-contained copy even while the
    /// relay keeps writing in WAL mode (it does not include the WAL side-file, so the
    /// snapshot is transaction-consistent). Written to a temp file, read back, deleted.
    pub fn snapshot_bytes(&self) -> Result<Vec<u8>> {
        let tmp = std::env::temp_dir()
            .join(format!("thetislink-relay-backup-{}.db", std::process::id()));
        // VACUUM INTO requires the target not to exist yet.
        let _ = std::fs::remove_file(&tmp);
        let path_lit = tmp.to_string_lossy().replace('\'', "''");
        self.conn
            .execute_batch(&format!("VACUUM INTO '{path_lit}'"))
            .context("VACUUM INTO snapshot")?;
        let bytes = std::fs::read(&tmp).context("reading snapshot file")?;
        let _ = std::fs::remove_file(&tmp);
        Ok(bytes)
    }
}

/// SHA-256 of the secret, hex-encoded. Adequate for high-entropy machine secrets:
/// brute-forcing a 256-bit random value is infeasible, so a slow KDF is not needed
/// (and would defeat the O(1) lookup-by-hash). Never store/log the plaintext.
pub fn hash_secret(secret: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Generate a fresh 256-bit station secret from the OS CSPRNG, hex-encoded.
pub fn generate_secret() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Hash a human admin password with Argon2id (security baseline: slow KDF, not SHA-256).
pub fn hash_password(password: &str) -> Result<String> {
    use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
    use argon2::Argon2;
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("argon2 hash failed: {e}"))
}

/// Verify a password against an Argon2 hash string. False on any mismatch/parse error.
pub fn verify_password(password: &str, hash: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    use argon2::Argon2;
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Store {
        Store::open(":memory:").expect("open in-memory store")
    }

    #[test]
    fn add_then_authenticate_roundtrip() {
        let s = mem();
        let secret = generate_secret();
        let id = s.add("PA3GHM", "owner", &secret, 100).unwrap();
        assert_eq!(s.authenticate(&secret).unwrap(), Some(id));
        // Unknown secret rejected.
        assert_eq!(s.authenticate("not-a-real-secret").unwrap(), None);
    }

    #[test]
    fn same_label_different_secret_are_distinct_rooms() {
        let s = mem();
        let a = generate_secret();
        let b = generate_secret();
        let id_a = s.add("PA3GHM", "", &a, 1).unwrap();
        let id_b = s.add("PA3GHM", "", &b, 2).unwrap();
        assert_ne!(id_a, id_b);
        assert_eq!(s.authenticate(&a).unwrap(), Some(id_a));
        assert_eq!(s.authenticate(&b).unwrap(), Some(id_b));
    }

    #[test]
    fn disabled_station_is_rejected() {
        let s = mem();
        let secret = generate_secret();
        let id = s.add("X", "", &secret, 1).unwrap();
        assert!(s.set_enabled(id, false).unwrap());
        assert_eq!(s.authenticate(&secret).unwrap(), None);
        assert!(s.set_enabled(id, true).unwrap());
        assert_eq!(s.authenticate(&secret).unwrap(), Some(id));
    }

    #[test]
    fn remove_and_count_and_list() {
        let s = mem();
        assert_eq!(s.count().unwrap(), 0);
        let secret = generate_secret();
        let id = s.add("X", "me", &secret, 42).unwrap();
        assert_eq!(s.count().unwrap(), 1);
        let rows = s.list().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "X");
        assert_eq!(rows[0].owner, "me");
        assert!(rows[0].enabled);
        assert_eq!(rows[0].created_at, 42);
        assert!(s.remove(id).unwrap());
        assert_eq!(s.count().unwrap(), 0);
        assert!(!s.remove(id).unwrap()); // already gone
    }

    #[test]
    fn plaintext_secret_is_never_stored() {
        let s = mem();
        let secret = generate_secret();
        s.add("X", "", &secret, 1).unwrap();
        // The stored hash must not equal the plaintext, and must equal hash_secret().
        let stored: String = s
            .conn
            .query_row("SELECT secret_hash FROM stations", [], |r| r.get(0))
            .unwrap();
        assert_ne!(stored, secret);
        assert_eq!(stored, hash_secret(&secret));
    }

    #[test]
    fn generated_secrets_are_unique_and_long() {
        let a = generate_secret();
        let b = generate_secret();
        assert_ne!(a, b);
        assert_eq!(a.len(), 64); // 32 bytes hex
    }

    #[test]
    fn admin_password_argon2_roundtrip() {
        let s = mem();
        assert!(!s.has_admin().unwrap());
        let hash = hash_password("s3cret-pw").unwrap();
        s.set_admin_password_hash(&hash, 1).unwrap();
        assert!(s.has_admin().unwrap());
        let stored = s.admin_password_hash().unwrap().unwrap();
        assert!(verify_password("s3cret-pw", &stored));
        assert!(!verify_password("wrong", &stored));
        assert!(stored.starts_with("$argon2")); // hashed, not plaintext
        assert!(!stored.contains("s3cret-pw"));
    }

    #[test]
    fn device_enroll_seen_seq_flags_and_name_keep() {
        let s = mem();
        let sec = generate_secret();
        let st = s.add("PA3GHM", "", &sec, 1).unwrap();
        let (d1, en1) = s
            .enroll_seen(st, "a-inst-1", Some("Pixel 7"), "a", Some("1.2.3.4"), 100)
            .unwrap();
        // New devices are enabled by default (auto-admit; the `enabled` flag is the
        // manual blocklist, there is no separate approval gate).
        assert!(en1);
        let (d2, _) = s
            .enroll_seen(st, "a-inst-2", Some("Galaxy"), "a", None, 100)
            .unwrap();
        assert_ne!(d1, d2);
        // Reconnect of d1 with no name: same id, name kept, ip+seen updated, NO session bump.
        let (d1b, _) = s
            .enroll_seen(st, "a-inst-1", None, "a", Some("5.6.7.8"), 200)
            .unwrap();
        assert_eq!(d1, d1b);
        let rows = s.list_devices(st).unwrap();
        assert_eq!(rows.len(), 2);
        let r1 = rows.iter().find(|r| r.install_id == "a-inst-1").unwrap();
        assert_eq!(r1.enroll_seq, 1);
        assert_eq!(r1.sessions, 0); // enroll_seen never bumps sessions
        assert_eq!(r1.last_seen, 200);
        assert_eq!(r1.name.as_deref(), Some("Pixel 7")); // kept: touch passed None
        assert_eq!(r1.last_ip.as_deref(), Some("5.6.7.8"));
        let r2 = rows.iter().find(|r| r.install_id == "a-inst-2").unwrap();
        assert_eq!(r2.enroll_seq, 2);
        // bump_session counts admitted sessions and refreshes last_seen.
        s.bump_session(d1, 300).unwrap();
        s.bump_session(d1, 400).unwrap();
        let r1 = s.list_devices(st).unwrap().into_iter().find(|r| r.id == d1).unwrap();
        assert_eq!(r1.sessions, 2);
        assert_eq!(r1.last_seen, 400);
        // device_admit_info reflects station + enabled flag; unknown id -> None.
        let (sid, iid, en) = s.device_admit_info(d1).unwrap().unwrap();
        assert_eq!(sid, st);
        assert_eq!(iid, "a-inst-1");
        assert!(en);
        assert!(s.device_admit_info(9999).unwrap().is_none());
    }

    #[test]
    fn device_bytes_rename_enable_remove() {
        let s = mem();
        let sec = generate_secret();
        let st = s.add("X", "", &sec, 1).unwrap();
        let (d, _) = s.enroll_seen(st, "inst", Some("dev"), "d", None, 1).unwrap();
        s.add_device_bytes(d, 1000).unwrap();
        s.add_device_bytes(d, 500).unwrap();
        assert_eq!(s.list_devices(st).unwrap()[0].bytes_total, 1500);
        assert!(s.rename_device(d, "kitchen").unwrap());
        assert_eq!(s.list_devices(st).unwrap()[0].name.as_deref(), Some("kitchen"));
        assert!(s.set_device_enabled(d, false).unwrap());
        assert!(!s.list_devices(st).unwrap()[0].enabled);
        assert!(s.remove_device(d).unwrap());
        assert!(s.list_devices(st).unwrap().is_empty());
    }

    #[test]
    fn station_monthly_usage_accumulates_and_lists() {
        let s = mem();
        let st = s.add("PA3GHM", "", &generate_secret(), 1).unwrap();
        assert_eq!(s.station_month_bytes(st, "2026-07").unwrap(), 0);
        s.add_station_month_bytes(st, "2026-07", 1000).unwrap();
        s.add_station_month_bytes(st, "2026-07", 500).unwrap();
        s.add_station_month_bytes(st, "2026-08", 200).unwrap();
        assert_eq!(s.station_month_bytes(st, "2026-07").unwrap(), 1500);
        assert_eq!(s.station_month_bytes(st, "2026-08").unwrap(), 200);
        // History is most-recent-month first.
        let hist = s.station_month_history(st, 12).unwrap();
        assert_eq!(hist, vec![("2026-08".into(), 200), ("2026-07".into(), 1500)]);
        // A different station is isolated.
        let st2 = s.add("OTHER", "", &generate_secret(), 1).unwrap();
        assert_eq!(s.station_month_bytes(st2, "2026-07").unwrap(), 0);
    }

    #[test]
    fn station_limits_and_enabled_count() {
        let s = mem();
        let st = s.add("X", "", &generate_secret(), 1).unwrap();
        assert_eq!(s.station_limits(st).unwrap(), (None, None, None));
        assert!(s.set_station_limits(st, Some(3), Some(2), Some(1000)).unwrap());
        assert_eq!(s.station_limits(st).unwrap(), (Some(3), Some(2), Some(1000)));
        // Enrolled devices count toward the max_devices cap immediately (auto-admit).
        let _ = s.enroll_seen(st, "i1", Some("a"), "a", None, 1).unwrap();
        let _ = s.enroll_seen(st, "i2", Some("b"), "a", None, 1).unwrap();
        assert_eq!(s.count_enabled_devices(st).unwrap(), 2);
        // Clearing limits sets them back to None.
        assert!(s.set_station_limits(st, None, None, None).unwrap());
        assert_eq!(s.station_limits(st).unwrap(), (None, None, None));
    }

    #[test]
    fn device_enabled_lookup_and_enabled_count() {
        let s = mem();
        let st = s.add("X", "", &generate_secret(), 1).unwrap();
        assert!(s.device_enabled(st, "nope").unwrap().is_none()); // brand-new device
        let (d1, _) = s.enroll_seen(st, "i1", Some("a"), "a", None, 1).unwrap();
        let (_d2, _) = s.enroll_seen(st, "i2", Some("b"), "a", None, 1).unwrap();
        assert_eq!(s.device_enabled(st, "i1").unwrap(), Some(true));
        assert_eq!(s.count_enabled_devices(st).unwrap(), 2);
        // A blocked device no longer counts toward the cap and reads as Some(false).
        s.set_device_enabled(d1, false).unwrap();
        assert_eq!(s.device_enabled(st, "i1").unwrap(), Some(false));
        assert_eq!(s.count_enabled_devices(st).unwrap(), 1);
    }

    #[test]
    fn rotate_secret_and_label() {
        let s = mem();
        let a = generate_secret();
        let id = s.add("X", "", &a, 1).unwrap();
        assert_eq!(s.authenticate(&a).unwrap(), Some(id));
        let b = generate_secret();
        assert!(s.rotate_secret(id, &b).unwrap());
        assert_eq!(s.authenticate(&a).unwrap(), None); // old secret dead
        assert_eq!(s.authenticate(&b).unwrap(), Some(id));
        assert!(s.set_label(id, "PA3GHM-new").unwrap());
        assert_eq!(s.list().unwrap()[0].label, "PA3GHM-new");
    }
}
