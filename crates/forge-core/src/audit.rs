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
        Self {
            id: Uuid::new_v4().to_string(),
            ts: Utc::now().timestamp_millis(),
            server: server.into(),
            tool: tool.into(),
            args_hash,
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
    pub fn default_path() -> Result<PathBuf> {
        Ok(supervisor::data_dir()?.join("audit.db"))
    }

    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let db_path = path.as_ref().to_owned();
        let conn = Connection::open(&db_path)
            .with_context(|| format!("failed to open audit database {}", db_path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS audit_events (
                 id          TEXT PRIMARY KEY,
                 ts          INTEGER NOT NULL,
                 server      TEXT NOT NULL,
                 tool        TEXT NOT NULL,
                 args_hash   TEXT,
                 result_code INTEGER,
                 latency_ms  INTEGER,
                 error       TEXT,
                 session_id  TEXT
             ) STRICT;
             CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit_events(ts);
             CREATE INDEX IF NOT EXISTS idx_audit_server ON audit_events(server);",
        )?;

        let (tx, rx) = channel::<AuditEvent>();
        let db_path_clone = db_path.clone();

        thread::Builder::new()
            .name("forge-audit-writer".to_string())
            .spawn(move || {
                let mut conn = Connection::open(&db_path_clone)
                    .expect("failed to open audit database in writer thread");

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
        let _ = self.tx.send(event);
    }
}

pub struct AuditReader {
    conn: Connection,
}

impl AuditReader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn =
            Connection::open(path).with_context(|| "failed to open audit database for reading")?;
        Ok(Self { conn })
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
            "SELECT id, ts, server, tool, args_hash, result_code, latency_ms, error, session_id
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
            params.push(Box::new(limit as i64));
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
                result_code: row.get(5)?,
                latency_ms: row.get::<_, i64>(6)? as u64,
                error: row.get(7)?,
                session_id: row.get(8)?,
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
             (id, ts, server, tool, args_hash, result_code, latency_ms, error, session_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )?;

        for event in batch {
            stmt.execute(params![
                event.id,
                event.ts,
                event.server,
                event.tool,
                event.args_hash,
                event.result_code,
                event.latency_ms as i64,
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
    use tempfile::NamedTempFile;

    #[test]
    fn audit_writer_writes_and_reader_reads() {
        let file = NamedTempFile::new().expect("failed to create temp file");
        let db_path = file.path().to_path_buf();

        let writer = AuditWriter::new(&db_path).expect("failed to create writer");
        let event = AuditEvent::new("local", "build", &Value::Null, 0, 123, None, None);
        writer.log(event.clone());

        // allow the writer thread to flush
        std::thread::sleep(std::time::Duration::from_millis(200));

        let reader = AuditReader::open(&db_path).expect("failed to open reader");
        let events = reader
            .query_events(AuditQuery::default(), Some(10))
            .expect("failed to query events");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].server, "local");
        assert_eq!(events[0].tool, "build");
        assert_eq!(events[0].result_code, 0);
    }
}
