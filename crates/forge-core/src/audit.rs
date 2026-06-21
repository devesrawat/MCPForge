use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, TryRecvError, channel};
use std::thread;
use uuid::Uuid;

use crate::supervisor;

#[derive(Debug, Clone, Serialize)]
pub struct AuditRecord {
    pub id: String,
    pub ts: i64,
    pub server: String,
    pub tool: String,
    pub args_hash: String,
    pub args_json: Option<String>,
    pub result_code: i32,
    pub latency_ms: u64,
    /// Sub-millisecond latency precision (microseconds). Present on v3+ databases.
    pub latency_us: Option<u64>,
    pub error: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct AuditQuery {
    pub server: Option<String>,
    pub tool: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub errors_only: bool,
}

#[derive(Debug, Clone)]
pub struct AuditEvent {
    pub id: String,
    pub ts: i64,
    pub server: String,
    pub tool: String,
    pub args_hash: String,
    pub args_json: Option<String>,
    pub result_code: i32,
    pub latency_ms: u64,
    /// Sub-millisecond latency precision (microseconds). Present on v3+ databases.
    pub latency_us: Option<u64>,
    pub error: Option<String>,
    pub session_id: Option<String>,
}

impl AuditEvent {
    pub fn new(
        server: impl Into<String>,
        tool: impl Into<String>,
        args: &Value,
        result_code: i32,
        latency_ms: u64,
        error: Option<String>,
        session_id: Option<String>,
    ) -> Self {
        Self::new_with_latency_us(
            server,
            tool,
            args,
            result_code,
            latency_ms.saturating_mul(1000),
            error,
            session_id,
        )
    }

    pub fn new_with_latency_us(
        server: impl Into<String>,
        tool: impl Into<String>,
        args: &Value,
        result_code: i32,
        latency_us: u64,
        error: Option<String>,
        session_id: Option<String>,
    ) -> Self {
        let args_hash = Self::hash_args(args);
        // Store full args only when FORGE_AUDIT_STORE_ARGS=1; otherwise scrub
        // sensitive keys before persisting to protect secrets at rest.
        let args_json = if std::env::var("FORGE_AUDIT_STORE_ARGS").as_deref() == Ok("1") {
            serde_json::to_string(args).ok()
        } else {
            let scrubbed = scrub_args(args);
            serde_json::to_string(&scrubbed).ok()
        };
        Self {
            id: Uuid::new_v4().to_string(),
            ts: Utc::now().timestamp_millis(),
            server: server.into(),
            tool: tool.into(),
            args_hash,
            args_json,
            result_code,
            latency_ms: latency_us / 1000,
            latency_us: Some(latency_us),
            error,
            session_id,
        }
    }

    /// Deprecated: use [`Self::new_with_latency_us`] instead.
    #[deprecated(since = "0.1.0", note = "use new_with_latency_us")]
    pub fn with_latency(
        server: impl Into<String>,
        tool: impl Into<String>,
        args: &Value,
        result_code: i32,
        latency_us: u64,
        error: Option<String>,
        session_id: Option<String>,
    ) -> Self {
        Self::new_with_latency_us(server, tool, args, result_code, latency_us, error, session_id)
    }

    fn hash_args(args: &Value) -> String {
        let args_text = serde_json::to_string(args).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(args_text.as_bytes());
        let digest = hasher.finalize();
        hex::encode(digest)
    }
}

/// Scrub sensitive keys from a JSON value before persisting to the audit log.
/// Keys whose names contain any of the sensitive patterns are replaced with
/// `"[redacted]"`. Non-object values are returned as-is.
fn scrub_args(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let scrubbed = map
                .iter()
                .map(|(k, v)| {
                    let v2 = if is_sensitive_key(k) {
                        Value::String("[redacted]".to_owned())
                    } else {
                        scrub_args(v)
                    };
                    (k.clone(), v2)
                })
                .collect();
            Value::Object(scrubbed)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(scrub_args).collect()),
        other => other.clone(),
    }
}

/// Returns true when a key name suggests it may hold a sensitive credential.
fn is_sensitive_key(key: &str) -> bool {
    let lower = key.to_lowercase();
    ["password", "token", "secret", "api_key", "auth", "key", "credential"]
        .iter()
        .any(|pattern| lower.contains(pattern))
}

pub struct AuditWriter {
    tx: Sender<AuditEvent>,
}

impl AuditWriter {
    /// Read the on-disk schema version and apply migrations if needed.
    /// Returns an error if the database was written by a newer version of forge.
    fn ensure_schema(conn: &Connection) -> Result<()> {
        // MAX(version) returns NULL (not QueryReturnedNoRows) on an empty table,
        // so a single query_row call always succeeds.
        let on_disk: Option<i64> = conn
            .query_row(
                "SELECT MAX(version) FROM schema_version",
                [],
                |r| r.get::<_, Option<i64>>(0),
            )
            .with_context(|| "failed to read schema_version from audit database")?;

        match on_disk {
            None => {
                // Fresh database — stamp the current version.
                conn.execute(
                    "INSERT INTO schema_version (version) VALUES (?1)",
                    rusqlite::params![Self::SCHEMA_VERSION],
                )?;
            }
            Some(v) if v > Self::SCHEMA_VERSION => {
                anyhow::bail!(
                    "audit database schema version {} is newer than this forge binary ({}); \\\
                     upgrade forge or delete the database",
                    v,
                    Self::SCHEMA_VERSION
                );
            }
            Some(2) => {
                // v2 → v3: add latency_us column for sub-millisecond precision.
                // Wrapped in a transaction so a crash mid-migration leaves the DB
                // in a consistent state (still at v2, not half-migrated).
                conn.execute_batch(
                    "BEGIN;
                     ALTER TABLE audit_events ADD COLUMN latency_us INTEGER;
                     UPDATE audit_events SET latency_us = latency_ms * 1000 WHERE latency_us IS NULL;
                     UPDATE schema_version SET version = 3;
                     COMMIT;",
                )?;
            }
            Some(1) => {
                // v1 → v3: add args_json and latency_us in a single transaction
                // so a crash at any point leaves the DB still at v1 (fully re-runnable).
                conn.execute_batch(
                    "BEGIN;
                     ALTER TABLE audit_events ADD COLUMN args_json TEXT;
                     ALTER TABLE audit_events ADD COLUMN latency_us INTEGER;
                     UPDATE audit_events SET latency_us = latency_ms * 1000 WHERE latency_us IS NULL;
                     UPDATE schema_version SET version = 3;
                     COMMIT;",
                )?;
            }
            Some(_) => {
                // Version matches or is older — no migration needed.
            }
        }
        Ok(())
    }

    pub fn default_path() -> Result<PathBuf> {
        Ok(supervisor::data_dir()?.join("audit.db"))
    }

    /// Current schema version. Bump this whenever the table layout changes and
    /// add a corresponding migration branch in `migrate_schema`.
    const SCHEMA_VERSION: i64 = 3;

    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let db_path = path.as_ref().to_owned();
        let conn = Connection::open(&db_path)
            .with_context(|| format!("failed to open audit database {}", db_path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS schema_version (
                 version INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS audit_events (
                 id          TEXT PRIMARY KEY,
                 ts          INTEGER NOT NULL,
                 server      TEXT NOT NULL,
                 tool        TEXT NOT NULL,
                 args_hash   TEXT,
                 args_json   TEXT,
                 result_code INTEGER,
                 latency_ms  INTEGER,
                 latency_us  INTEGER,
                 error       TEXT,
                 session_id  TEXT
             ) STRICT;
             CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit_events(ts);
             CREATE INDEX IF NOT EXISTS idx_audit_server ON audit_events(server);",
        )?;
        Self::ensure_schema(&conn)?;

        let (tx, rx) = channel::<AuditEvent>();

        thread::Builder::new()
            .name("forge-audit-writer".to_string())
            .spawn(move || {
                let mut conn = conn;

                while let Ok(event) = rx.recv() {
                    let mut batch = vec![event];
                    while batch.len() < 100 {
                        match rx.try_recv() {
                            Ok(next_event) => batch.push(next_event),
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => break,
                        }
                    }
                    if let Err(err) = insert_batch(&mut conn, &batch) {
                        eprintln!("audit write failed: {}", err);
                    }
                }
            })?;

        Ok(Self { tx })
    }

    pub fn log(&self, event: AuditEvent) {
        if self.tx.send(event).is_err() {
            tracing::warn!("audit writer channel disconnected — event dropped");
        }
    }
}

pub struct AuditReader {
    conn: Connection,
}

impl AuditReader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn =
            Connection::open(path).with_context(|| "failed to open audit database for reading")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS audit_events (
                 id          TEXT PRIMARY KEY,
                 ts          INTEGER NOT NULL,
                 server      TEXT NOT NULL,
                 tool        TEXT NOT NULL,
                 args_hash   TEXT,
                 args_json   TEXT,
                 result_code INTEGER,
                 latency_ms  INTEGER,
                 latency_us  INTEGER,
                 error       TEXT,
                 session_id  TEXT
             ) STRICT;
             CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit_events(ts);
             CREATE INDEX IF NOT EXISTS idx_audit_server ON audit_events(server);",
        )
        .with_context(|| "failed to initialize audit database schema")?;
        Self::migrate_if_needed(&conn)?;
        Ok(Self { conn })
    }

    fn migrate_if_needed(conn: &Connection) -> Result<()> {
        // MAX(version) returns NULL on an empty table — no need to special-case
        // QueryReturnedNoRows. ORDER BY / MAX also prevents a stale low-version
        // row from being picked up if the table ever has duplicates.
        let on_disk: Option<i64> = conn
            .query_row(
                "SELECT MAX(version) FROM schema_version",
                [],
                |r| r.get::<_, Option<i64>>(0),
            )
            .with_context(|| "failed to read schema_version from audit database")?;

        match on_disk {
            None => {
                conn.execute(
                    "INSERT INTO schema_version (version) VALUES (?1)",
                    rusqlite::params![AuditWriter::SCHEMA_VERSION],
                )?;
            }
            Some(1) => {
                // v1 → v3 in one transaction: crash at any point leaves DB at v1.
                conn.execute_batch(
                    "BEGIN;
                     ALTER TABLE audit_events ADD COLUMN args_json TEXT;
                     ALTER TABLE audit_events ADD COLUMN latency_us INTEGER;
                     UPDATE audit_events SET latency_us = latency_ms * 1000 WHERE latency_us IS NULL;
                     UPDATE schema_version SET version = 3;
                     COMMIT;",
                )?;
            }
            Some(2) => {
                // v2 → v3 in one transaction.
                conn.execute_batch(
                    "BEGIN;
                     ALTER TABLE audit_events ADD COLUMN latency_us INTEGER;
                     UPDATE audit_events SET latency_us = latency_ms * 1000 WHERE latency_us IS NULL;
                     UPDATE schema_version SET version = 3;
                     COMMIT;",
                )?;
            }
            Some(_) => {}
        }
        Ok(())
    }

    pub fn open_default() -> Result<Self> {
        Self::open(AuditWriter::default_path()?)
    }

    pub fn query_events(
        &self,
        query: AuditQuery,
        limit: Option<usize>,
    ) -> Result<Vec<AuditRecord>> {
        let mut sql = String::from(
            "SELECT id, ts, server, tool, args_hash, args_json, result_code, latency_ms, latency_us, error, session_id
             FROM audit_events
             WHERE 1=1",
        );
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(server) = query.server {
            sql.push_str(" AND server = ?");
            params.push(Box::new(server));
        }
        if let Some(tool) = query.tool {
            sql.push_str(" AND tool = ?");
            params.push(Box::new(tool));
        }
        if let Some(since) = query.since {
            sql.push_str(" AND ts >= ?");
            params.push(Box::new(since.timestamp_millis()));
        }
        if query.errors_only {
            sql.push_str(" AND result_code <> 0");
        }

        sql.push_str(" ORDER BY ts DESC");
        if let Some(limit) = limit {
            sql.push_str(" LIMIT ?");
            params.push(Box::new(i64::try_from(limit).unwrap_or(i64::MAX)));
        }

        let mut stmt = self.conn.prepare(&sql)?;
        let params_refs = params_from_vec(&params);
        let rows = stmt.query_map(params_refs.as_slice(), row_to_audit_record)?;

        rows.collect::<Result<Vec<_>, rusqlite::Error>>()
            .map_err(|err| anyhow!(err))
    }

    /// Fetch events newer than the given timestamp. Used by `forge watch`
    /// to poll for new records and render them live in the TUI.
    pub fn poll_new(&self, last_ts: i64, max_rows: usize) -> Result<Vec<AuditRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, server, tool, args_hash, args_json, result_code, latency_ms, latency_us, error, session_id
             FROM audit_events
             WHERE ts > ?
             ORDER BY ts ASC
             LIMIT ?",
        )?;
        let rows = stmt.query_map(
            params![last_ts, i64::try_from(max_rows).unwrap_or(i64::MAX)],
            row_to_audit_record,
        )?;

        rows.collect::<Result<Vec<_>, rusqlite::Error>>()
            .map_err(|err| anyhow!(err))
    }
}

fn row_to_audit_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<AuditRecord> {
    let latency_us = row
        .get::<_, Option<i64>>(8)?
        .map(|v| u64::try_from(v).unwrap_or(0));
    Ok(AuditRecord {
        id: row.get(0)?,
        ts: row.get(1)?,
        server: row.get(2)?,
        tool: row.get(3)?,
        args_hash: row.get(4)?,
        args_json: row.get(5)?,
        result_code: row.get(6)?,
        latency_ms: u64::try_from(row.get::<_, i64>(7)?).unwrap_or(0),
        latency_us,
        error: row.get(9)?,
        session_id: row.get(10)?,
    })
}

/// Format latency for CLI display, using microsecond precision when available.
pub fn format_latency(record: &AuditRecord) -> String {
    match record.latency_us {
        Some(us) => format!("{:.2}ms", us as f64 / 1000.0),
        None => format!("{}ms", record.latency_ms),
    }
}

/// Latency in floating-point milliseconds.
pub fn latency_ms_f64(record: &AuditRecord) -> f64 {
    record
        .latency_us
        .map(|us| us as f64 / 1000.0)
        .unwrap_or(record.latency_ms as f64)
}

fn params_from_vec(params: &[Box<dyn rusqlite::ToSql>]) -> Vec<&dyn rusqlite::ToSql> {
    params
        .iter()
        .map(|value| value.as_ref() as &dyn rusqlite::ToSql)
        .collect()
}

fn insert_batch(conn: &mut Connection, batch: &[AuditEvent]) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT OR IGNORE INTO audit_events
             (id, ts, server, tool, args_hash, args_json, result_code, latency_ms, latency_us, error, session_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )?;

        for event in batch {
            stmt.execute(params![
                event.id,
                event.ts,
                event.server,
                event.tool,
                event.args_hash,
                event.args_json,
                event.result_code,
                i64::try_from(event.latency_ms).unwrap_or(i64::MAX),
                event
                    .latency_us
                    .map(|v| i64::try_from(v).unwrap_or(i64::MAX)),
                event.error,
                event.session_id,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use tempfile::NamedTempFile;

    #[test]
    fn audit_writer_writes_and_reader_reads() {
        let file = NamedTempFile::new().expect("failed to create temp file");
        let db_path = file.path().to_path_buf();

        let writer = AuditWriter::new(&db_path).expect("failed to create writer");
        let event = AuditEvent::new("local", "build", &Value::Null, 0, 123, None, None);
        writer.log(event.clone());
        drop(writer);

        let reader = AuditReader::open(&db_path).expect("failed to open reader");
        let deadline = Instant::now() + Duration::from_secs(3);
        let events = loop {
            let events = reader
                .query_events(AuditQuery::default(), Some(10))
                .expect("failed to query events");
            if !events.is_empty() {
                break events;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for audit writer flush"
            );
            std::thread::sleep(Duration::from_millis(25));
        };

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].server, "local");
        assert_eq!(events[0].tool, "build");
        assert_eq!(events[0].result_code, 0);
    }

    #[test]
    fn audit_event_stores_args_json() {
        let file = NamedTempFile::new().expect("temp file");
        let db_path = file.path().to_path_buf();

        let writer = AuditWriter::new(&db_path).expect("writer");
        let args = Value::String("hello".to_string());
        let event = AuditEvent::new("gh", "search", &args, 0, 50, None, None);
        assert!(event.args_json.is_some());
        assert_eq!(event.args_json.as_deref(), Some(r#""hello""#));
        writer.log(event);
        drop(writer);

        let reader = AuditReader::open(&db_path).expect("reader");
        let deadline = Instant::now() + Duration::from_secs(3);
        let events = loop {
            let events = reader
                .query_events(AuditQuery::default(), Some(10))
                .expect("query failed");
            if !events.is_empty() {
                break events;
            }
            assert!(Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(25));
        };

        assert_eq!(events.len(), 1);
        assert!(events[0].args_json.is_some());
        assert_eq!(events[0].args_json.as_deref(), Some(r#""hello""#));
    }

    #[test]
    fn poll_new_returns_events_after_last_ts() {
        let file = NamedTempFile::new().expect("temp file");
        let db_path = file.path().to_path_buf();

        // Create and write events directly (no writer thread).
        let conn = Connection::open(&db_path).expect("open");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS audit_events (
                 id TEXT PRIMARY KEY, ts INTEGER NOT NULL, server TEXT NOT NULL,
                 tool TEXT NOT NULL, args_hash TEXT, args_json TEXT,
                 result_code INTEGER, latency_ms INTEGER, error TEXT, session_id TEXT
             ) STRICT;
             CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit_events(ts);",
        )
        .expect("exec");
        conn.execute("INSERT INTO schema_version (version) VALUES (2)", [])
            .expect("stamp");

        let mut stmt = conn
            .prepare(
                "INSERT INTO audit_events (id, ts, server, tool, args_hash, args_json, result_code, latency_ms)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .expect("prepare");
        stmt.execute(params![
            "e1",
            1000,
            "alpha",
            "t1",
            "h1",
            r#"{"x":1}"#,
            0,
            10
        ])
        .expect("ins");
        stmt.execute(params!["e2", 2000, "beta", "t2", "h2", r#"{"x":2}"#, 0, 20])
            .expect("ins");
        stmt.execute(params![
            "e3",
            3000,
            "alpha",
            "t3",
            "h3",
            r#"{"x":3}"#,
            -1,
            30
        ])
        .expect("ins");

        let reader = AuditReader::open(&db_path).expect("reader");

        // Poll after ts=0 — should get all 3.
        let all = reader.poll_new(0, 100).expect("poll");
        assert_eq!(all.len(), 3);

        // Poll after ts=1500 — should get e2, e3.
        let some = reader.poll_new(1500, 100).expect("poll");
        assert_eq!(some.len(), 2);
        assert_eq!(some[0].id, "e2");
        assert_eq!(some[1].id, "e3");

        // Poll after ts=3000 — should get none.
        let none = reader.poll_new(3000, 100).expect("poll");
        assert!(none.is_empty());

        // Respect max_rows.
        let one = reader.poll_new(0, 1).expect("poll");
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].id, "e1");
    }

    #[test]
    fn query_events_filters_by_error() {
        let file = NamedTempFile::new().expect("temp file");
        let db_path = file.path().to_path_buf();

        let writer = AuditWriter::new(&db_path).expect("writer");
        let e1 = AuditEvent::new("s", "t1", &Value::Null, 0, 10, None, None);
        let e2 = AuditEvent::new("s", "t2", &Value::Null, -1, 20, Some("oops".into()), None);
        writer.log(e1);
        writer.log(e2);
        drop(writer);

        let reader = AuditReader::open(&db_path).expect("reader");
        let deadline = Instant::now() + Duration::from_secs(3);
        let events = loop {
            let events = reader
                .query_events(AuditQuery::default(), Some(10))
                .expect("query");
            if events.len() >= 2 {
                break events;
            }
            assert!(Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(25));
        };
        assert_eq!(events.len(), 2);

        let errors = reader
            .query_events(
                AuditQuery {
                    errors_only: true,
                    ..Default::default()
                },
                Some(10),
            )
            .expect("errors");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].tool, "t2");
    }

    /// Verify AuditReader migrates a v1 schema (no args_json, no latency_us) up
    /// to the current v3 schema without data loss.
    #[test]
    fn migrate_if_needed_v1_to_v3() {
        let file = NamedTempFile::new().expect("temp file");
        let db_path = file.path().to_path_buf();

        // Create a v1 database manually (no args_json or latency_us columns).
        let conn = Connection::open(&db_path).expect("open");
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER NOT NULL);
             CREATE TABLE audit_events (
                 id TEXT PRIMARY KEY,
                 ts INTEGER NOT NULL,
                 server TEXT NOT NULL,
                 tool TEXT NOT NULL,
                 args_hash TEXT,
                 result_code INTEGER,
                 latency_ms INTEGER,
                 error TEXT,
                 session_id TEXT
             ) STRICT;
             INSERT INTO schema_version (version) VALUES (1);
             INSERT INTO audit_events (id, ts, server, tool, args_hash, result_code, latency_ms)
             VALUES ('v1-row', 1000, 'srv', 'op', 'h1', 0, 42);",
        )
        .expect("setup v1 db");
        drop(conn);

        // Opening via AuditReader should migrate v1 → v3 in one transaction.
        let reader = AuditReader::open(&db_path).expect("migrate open");

        // Verify the migrated event can be read back with the new columns present.
        let events = reader
            .query_events(AuditQuery::default(), Some(10))
            .expect("query after migration");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, "v1-row");
        assert_eq!(events[0].server, "srv");
        assert_eq!(events[0].tool, "op");
        assert_eq!(events[0].latency_ms, 42);
        // latency_us should have been back-filled: 42 ms × 1000 = 42 000 µs.
        assert_eq!(events[0].latency_us, Some(42_000));
        // args_json is NULL in the v1 row — should come back as None.
        assert!(events[0].args_json.is_none());
    }

    /// Verify that MAX(version) is used rather than an arbitrary LIMIT 1 —
    /// a database with two rows in schema_version should report the higher one.
    #[test]
    fn migrate_if_needed_reads_max_version() {
        let file = NamedTempFile::new().expect("temp file");
        let db_path = file.path().to_path_buf();

        let conn = Connection::open(&db_path).expect("open");
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER NOT NULL);
             CREATE TABLE audit_events (
                 id TEXT PRIMARY KEY,
                 ts INTEGER NOT NULL,
                 server TEXT NOT NULL,
                 tool TEXT NOT NULL,
                 args_hash TEXT,
                 args_json TEXT,
                 result_code INTEGER,
                 latency_ms INTEGER,
                 latency_us INTEGER,
                 error TEXT,
                 session_id TEXT
             ) STRICT;
             -- Simulate a DB that was partially written with two version rows.
             -- The migration should see MAX = 3 (current) and skip migrations.
             INSERT INTO schema_version (version) VALUES (1);
             INSERT INTO schema_version (version) VALUES (3);",
        )
        .expect("setup");
        drop(conn);

        // Should not attempt migrations (which would fail on already-existing columns).
        AuditReader::open(&db_path).expect("should open cleanly with max version = 3");
    }
}
