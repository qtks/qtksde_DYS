//! Auxiliary schema + connection helpers for the AI layer.
//!
//! The AI layer never opens the user's files for writing. Index writes are
//! limited to tables declared here.

use std::path::Path;

use rusqlite::Connection;

use crate::{IndexError, Result};

/// Shared with `walker-index`, which owns the migration chain. Keeping one
/// copy means `open_rw` and the schema migration can never disagree.
pub const AI_SCHEMA_SQL: &str = walker_index::M3_SCHEMA_SQL;

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
