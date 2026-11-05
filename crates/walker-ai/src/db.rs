//! Auxiliary schema + connection helpers for the AI layer.
//!
//! The AI layer never opens the user's files for writing. Index writes are
//! limited to tables declared here.

use std::path::Path;

use rusqlite::Connection;

use crate::{IndexError, Result};

pub const AI_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS projects (
    id         TEXT PRIMARY KEY,
    volume_id  TEXT NOT NULL,
    root_path  TEXT NOT NULL,
    kind       TEXT NOT NULL,
    markers    TEXT NOT NULL,
    depth      INTEGER NOT NULL,
    file_count INTEGER NOT NULL DEFAULT 0,
    bytes      INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL
) WITHOUT ROWID;
CREATE UNIQUE INDEX IF NOT EXISTS idx_projects_root ON projects(volume_id, root_path);

CREATE TABLE IF NOT EXISTS vectors (
    file_id      TEXT PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
    shard        TEXT NOT NULL,
    dim          INTEGER NOT NULL,
    vec          BLOB NOT NULL,
    content_hash TEXT NOT NULL,
    mtime        INTEGER NOT NULL,
    size         INTEGER NOT NULL,
    model        TEXT NOT NULL,
    updated_at   TEXT NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_vectors_shard ON vectors(shard);

CREATE TABLE IF NOT EXISTS vector_shards (
    shard      TEXT PRIMARY KEY,
    volume_id  TEXT NOT NULL,
    top_dir    TEXT NOT NULL,
    rows       INTEGER NOT NULL,
    dim        INTEGER NOT NULL,
    model      TEXT NOT NULL,
    built_at   TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS archive_suggestions (
    id          TEXT PRIMARY KEY,
    kind        TEXT NOT NULL,
    action      TEXT NOT NULL,
    paths       TEXT NOT NULL,
    evidence    TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'pending',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_suggestions_status ON archive_suggestions(status, created_at);
"#;

pub fn open_ro(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    conn.execute_batch(
        "PRAGMA query_only=ON;
         PRAGMA temp_store=MEMORY;
         PRAGMA cache_size=-32768;",
    )?;
    Ok(conn)
}

pub fn open_rw(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA temp_store=MEMORY;
         PRAGMA cache_size=-32768;",
    )?;
    ensure_schema(&conn)?;
    Ok(conn)
}

pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(AI_SCHEMA_SQL)?;
    Ok(())
}

pub fn now_iso(conn: &Connection) -> String {
    conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ','now')", [], |row| {
        row.get::<_, String>(0)
    })
    .unwrap_or_else(|_| "1970-01-01T00:00:00.000Z".to_string())
}

pub fn config_error(message: impl Into<String>) -> IndexError {
    IndexError::Config(message.into())
}
