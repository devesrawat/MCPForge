use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
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
        let args_hash = Self::hash_args(args);
        let args_json = serde_json::to_string(args).ok();
        Self {
            id: Uuid::new_v4().to_string(),
            ts: Utc::now().timestamp_millis(),
            server: server.into(),
            tool: tool.into(),
            args_hash,
            args_json,
            result_code,
            latency_ms,
            error,
            session_id,
        }
    }

    fn hash_args(args: &Value) -> String {
        let args_text = serde_json::to_string(args).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(args_text.as_bytes());
        let digest = hasher.finalize();
        hex::encode(digest)
    }
}

pub struct AuditWriter {
    tx: Sender<AuditEvent>,
}

impl AuditWriter {
    /// Read the on-disk schema version and apply migrations if needed.
    /// Returns an error if the database was written by a newer version of forge.
    fn ensure_schema(conn: &Connection) -> Result<()> {
        let on_disk: Option<i64> =
            match conn.query_row("SELECT version FROM schema_version LIMIT 1", [], |r| {
                r.get(0)
            }) {
                Ok(v) => Some(v),
                Err(rusqlite::Error::QueryReturnedNoRows) => None,
                Err(e) => {
                    return Err(
                        anyhow!(e).context("failed to read schema_version from audit database")
                    );
                }
            };

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
            Some(1) => {
                // v1 → v2: add args_json column.
                conn.execute(
                    "ALTER TABLE audit_events ADD COLUMN args_json TEXT",
                    [],
                )?;
                conn.execute(
                    "UPDATE schema_version SET version = ?1",
                    rusqlite::params![2],
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
    const SCHEMA_VERSION: i64 = 2;

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
        let on_disk: Option<i64> =
            match conn.query_row("SELECT version FROM schema_version LIMIT 1", [], |r| {
                r.get(0)
            }) {
                Ok(v) => Some(v),
                Err(rusqlite::Error::QueryReturnedNoRows) => None,
                Err(_) => Some(0),
            };
        if on_disk == Some(1) {
            conn.execute(
                "ALTER TABLE audit_events ADD COLUMN args_json TEXT",
                [],
            )?;
            conn.execute(
                "UPDATE schema_version SET version = ?1",
                rusqlite::params![2],
            )?;
        } else if on_disk.is_none() {
            conn.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                rusqlite::params![2],
            )?;
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
            "SELECT id, ts, server, tool, args_hash, args_json, result_code, latency_ms, error, session_id
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
        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            Ok(AuditRecord {
                id: row.get(0)?,
                ts: row.get(1)?,
                server: row.get(2)?,
                tool: row.get(3)?,
                args_hash: row.get(4)?,
                args_json: row.get(5)?,
                result_code: row.get(6)?,
                latency_ms: u64::try_from(row.get::<_, i64>(7)?).unwrap_or(0),
                error: row.get(8)?,
                session_id: row.get(9)?,
            })
        })?;

        rows.collect::<Result<Vec<_>, rusqlite::Error>>()
            .map_err(|err| anyhow!(err))
    }

    /// Fetch events newer than the given timestamp. Used by `forge watch`
    /// to poll for new records and render them live in the TUI.
    pub fn poll_new(&self, last_ts: i64, max_rows: usize) -> Result<Vec<AuditRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, server, tool, args_hash, args_json, result_code, latency_ms, error, session_id
             FROM audit_events
             WHERE ts > ?
             ORDER BY ts ASC
             LIMIT ?",
        )?;
        let rows = stmt.query_map(params![last_ts, i64::try_from(max_rows).unwrap_or(i64::MAX)], |row| {
            Ok(AuditRecord {
                id: row.get(0)?,
                ts: row.get(1)?,
                server: row.get(2)?,
                tool: row.get(3)?,
                args_hash: row.get(4)?,
                args_json: row.get(5)?,
                result_code: row.get(6)?,
                latency_ms: u64::try_from(row.get::<_, i64>(7)?).unwrap_or(0),
                error: row.get(8)?,
                session_id: row.get(9)?,
            })
        })?;

        rows.collect::<Result<Vec<_>, rusqlite::Error>>()
            .map_err(|err| anyhow!(err))
    }
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
             (id, ts, server, tool, args_hash, args_json, result_code, latency_ms, error, session_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
        let mut conn = Connection::open(&db_path).expect("open");
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
        conn.execute(
            "INSERT INTO schema_version (version) VALUES (2)",
            [],
        )
        .expect("stamp");

        let mut stmt = conn
            .prepare(
                "INSERT INTO audit_events (id, ts, server, tool, args_hash, args_json, result_code, latency_ms)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .expect("prepare");
        stmt.execute(params!["e1", 1000, "alpha", "t1", "h1", r#"{"x":1}"#, 0, 10])
            .expect("ins");
        stmt.execute(params!["e2", 2000, "beta", "t2", "h2", r#"{"x":2}"#, 0, 20])
            .expect("ins");
        stmt.execute(params!["e3", 3000, "alpha", "t3", "h3", r#"{"x":3}"#, -1, 30])
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
}
