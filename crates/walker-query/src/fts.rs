use std::time::Instant;

use rusqlite::{params, Connection};
use serde::Serialize;

use crate::Result;

pub const FTS_TABLE: &str = "files_fts";

/// External-content FTS5 table. `content='files'` means the index stores only
/// trigram postings; the actual rows still live in `files`, so no text is
/// duplicated. `norm_name` is the NFC-normalized name, matching exact/prefix
/// semantics. The builtin `trigram` tokenizer needs no custom tokenizer and
/// gives substring matching for CJK (>= 3 characters).
fn create_sql() -> String {
    format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS {FTS_TABLE} USING fts5(
             norm_name,
             content = 'files',
             content_rowid = 'rowid',
             tokenize = 'trigram'
         )"
    )
}

#[derive(Debug, Clone, Serialize)]
pub struct FtsStatus {
    pub table_exists: bool,
    pub indexed_rows: u64,
    pub files_rows: u64,
    pub online_rows: u64,
    pub stale: bool,
    pub built_at: Option<String>,
    pub build_ms: Option<u64>,
    pub page_size: u64,
    pub page_count: u64,
    pub index_bytes_estimate: u64,
}

pub fn fts_status(conn: &Connection) -> Result<FtsStatus> {
    let table_exists: bool = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE name = ?1",
        params![FTS_TABLE],
        |row| row.get::<_, i64>(0),
    )? > 0;

    let files_rows: u64 =
        conn.query_row("SELECT COUNT(*) FROM files", [], |row| row.get::<_, i64>(0))? as u64;
    let online_rows: u64 = conn.query_row(
        "SELECT COUNT(*) FROM files WHERE status='online'",
        [],
        |row| row.get::<_, i64>(0),
    )? as u64;

    let meta = |key: &str| -> Result<Option<String>> {
        let value: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key=?1", params![key], |row| {
                row.get(0)
            })
            .ok();
        Ok(value)
    };
    // Staleness is tracked from the count recorded at build time. `count(*)`
    // on an external-content FTS table falls back to the content table, so it
    // cannot tell us whether the index itself saw the new rows.
    let indexed_rows: u64 = if table_exists {
        meta("fts_rows")?
            .and_then(|value| value.parse().ok())
            .unwrap_or(0)
    } else {
        0
    };
    let built_at = meta("fts_built_at")?;
    let build_ms = meta("fts_build_ms")?.and_then(|value| value.parse().ok());
    let page_size: u64 = conn.query_row("PRAGMA page_size", [], |row| row.get::<_, i64>(0))? as u64;
    let page_count: u64 =
        conn.query_row("PRAGMA page_count", [], |row| row.get::<_, i64>(0))? as u64;

    Ok(FtsStatus {
        table_exists,
        indexed_rows,
        files_rows,
        online_rows,
        // Trigram triggers are not used: the index is rebuilt in batches.
        // A stale index is therefore possible after incremental scans and is
        // reported instead of silently returning partial results.
        stale: table_exists && indexed_rows != files_rows,
        built_at,
        build_ms,
        page_size,
        page_count,
        index_bytes_estimate: page_size.saturating_mul(page_count),
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct FtsBuildReport {
    pub table: &'static str,
    pub tokenizer: &'static str,
    pub rows: u64,
    pub build_ms: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub index_bytes: u64,
    pub bytes_per_row: f64,
    pub checkpoint_ms: u64,
}

/// Rebuild the whole trigram index in one statement.
///
/// Sync policy: **batch rebuild, not triggers.** Triggers would fire for every
/// row of every incremental scan (1M extra FTS inserts during a fresh load,
/// and they would have to be maintained while the bulk path is dropping
/// indexes). Instead the index is marked with `fts_built_at`/`fts_build_ms`
/// and `fts_status` compares its row count with the live row count so staleness
/// is visible.
///
/// Crash behaviour: `INSERT INTO files_fts(files_fts) VALUES('rebuild')` runs
/// inside a single transaction together with the meta update. If the process
/// dies mid-rebuild, SQLite rolls the transaction back and the previous index
/// state remains, with the old `fts_built_at` value. Re-running is safe and
/// idempotent.
pub fn rebuild_fts(conn: &Connection) -> Result<FtsBuildReport> {
    conn.execute_batch(&create_sql())?;

    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    let page_size: u64 = conn.query_row("PRAGMA page_size", [], |row| row.get::<_, i64>(0))? as u64;
    let bytes_before =
        page_size * conn.query_row("PRAGMA page_count", [], |row| row.get::<_, i64>(0))? as u64;

    let started = Instant::now();
    let transaction = conn.unchecked_transaction()?;
    transaction.execute(
        &format!("INSERT INTO {FTS_TABLE}({FTS_TABLE}) VALUES('rebuild')"),
        [],
    )?;
    let rows: u64 =
        transaction.query_row("SELECT COUNT(*) FROM files", [], |row| row.get::<_, i64>(0))? as u64;
    let build_ms = started.elapsed().as_millis() as u64;
    let now = crate::treemap::now_iso8601();
    for (key, value) in [
        ("fts_built_at", now),
        ("fts_build_ms", build_ms.to_string()),
        ("fts_rows", rows.to_string()),
    ] {
        transaction.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
    }
    transaction.commit()?;

    let checkpoint_started = Instant::now();
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    let checkpoint_ms = checkpoint_started.elapsed().as_millis() as u64;
    let bytes_after =
        page_size * conn.query_row("PRAGMA page_count", [], |row| row.get::<_, i64>(0))? as u64;

    Ok(FtsBuildReport {
        table: FTS_TABLE,
        tokenizer: "trigram",
        rows,
        build_ms,
        bytes_before,
        bytes_after,
        index_bytes: bytes_after.saturating_sub(bytes_before),
        bytes_per_row: if rows == 0 {
            0.0
        } else {
            bytes_after.saturating_sub(bytes_before) as f64 / rows as f64
        },
        checkpoint_ms,
    })
}
