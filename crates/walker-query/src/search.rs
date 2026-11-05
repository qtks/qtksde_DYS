use std::time::Instant;

use rusqlite::{params, Connection};
use serde::Serialize;
use walker_core::normalize_name;

use crate::{Result, FTS_TABLE};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchKind {
    Exact,
    Prefix,
    Fts,
}

impl SearchKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "exact" => Some(Self::Exact),
            "prefix" => Some(Self::Prefix),
            "fts" => Some(Self::Fts),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PrefixMode {
    /// `norm_name >= q AND norm_name < q || U+10FFFF` — pure index range scan.
    Range,
    /// `norm_name LIKE 'q%'` with `case_sensitive_like=ON` so SQLite can use the
    /// BINARY index. Measured against the range form.
    Like,
}

impl PrefixMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "range" => Some(Self::Range),
            "like" => Some(Self::Like),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub kind: SearchKind,
    /// Raw user query; normalized to NFC before matching, exactly like
    /// `norm_name` was at index time.
    pub query: String,
    pub limit: usize,
    pub prefix_mode: PrefixMode,
    pub volume_id: Option<String>,
    pub offset: usize,
    /// FTS only: order by bm25 relevance. Turning it off lets SQLite stop
    /// after the first `limit` matches (rank has to score every match first).
    pub rank: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            kind: SearchKind::Exact,
            query: String::new(),
            limit: 100,
            prefix_mode: PrefixMode::Range,
            volume_id: None,
            offset: 0,
            rank: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub id: String,
    pub path: String,
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    pub mtime: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub kind: SearchKind,
    /// Which access path ran: `exact_index`, `prefix_range`, `prefix_like`,
    /// `fts_trigram`, `fts_like_fallback`.
    pub strategy: String,
    pub norm_query: String,
    pub limit: usize,
    pub offset: usize,
    pub hits: Vec<SearchHit>,
    pub took_us: u64,
    pub plan: Vec<String>,
    pub note: Option<String>,
}

const HIT_COLUMNS: &str = "id, path, name, size, is_dir, mtime";

/// All statements use fixed placeholders: ?1 query, ?2 volume-or-null,
/// ?3 limit, ?4 offset.
pub fn search(conn: &Connection, options: &SearchOptions) -> Result<SearchResult> {
    let norm_query = normalize_name(&options.query);
    if norm_query.is_empty() {
        return Err(crate::IndexError::Config("empty query".to_string()));
    }
    let limit = options.limit.clamp(1, 5_000_000);
    let started = Instant::now();
    let volume = options.volume_id.clone();

    let (sql, bind_query, strategy, note) = match options.kind {
        SearchKind::Exact => (
            exact_sql(),
            norm_query.clone(),
            "exact_index".to_string(),
            None,
        ),
        SearchKind::Prefix => match options.prefix_mode {
            PrefixMode::Range => (
                prefix_range_sql(),
                norm_query.clone(),
                "prefix_range".to_string(),
                None,
            ),
            PrefixMode::Like => {
                conn.execute_batch("PRAGMA case_sensitive_like=ON;")?;
                (
                    prefix_like_sql(),
                    format!("{norm_query}%"),
                    "prefix_like".to_string(),
                    None,
                )
            }
        },
        SearchKind::Fts => {
            if !table_exists(conn, FTS_TABLE)? {
                return Err(crate::IndexError::Config(format!(
                    "{FTS_TABLE} is missing; run `walker-query fts-build --db <path>` first"
                )));
            }
            // A trigram tokenizer cannot answer queries shorter than 3
            // characters: fall back to a LIKE scan and say so explicitly.
            if norm_query.chars().count() < 3 {
                (
                    fts_like_sql(),
                    norm_query.clone(),
                    "fts_like_fallback".to_string(),
                    Some("query shorter than 3 chars: trigram index cannot be used".to_string()),
                )
            } else {
                (
                    fts_match_sql(options.rank),
                    format!("\"{}\"", norm_query.replace('"', "\"\"")),
                    if options.rank {
                        "fts_trigram".to_string()
                    } else {
                        "fts_trigram_unranked".to_string()
                    },
                    None,
                )
            }
        }
    };

    let plan = explain(
        conn,
        &sql,
        &bind_query,
        volume.as_deref(),
        limit,
        options.offset,
    )?;
    let mut statement = conn.prepare(&sql)?;
    let mapped = statement.query_map(
        params![bind_query, volume, limit as i64, options.offset as i64],
        map_hit,
    )?;
    let hits = mapped.collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(SearchResult {
        kind: options.kind,
        strategy,
        norm_query,
        limit,
        offset: options.offset,
        hits,
        took_us: started.elapsed().as_micros() as u64,
        plan,
        note,
    })
}

fn volume_filter(alias: &str) -> String {
    format!(" AND (?2 IS NULL OR {alias}.volume_id = ?2)")
}

fn exact_sql() -> String {
    format!(
        "SELECT {HIT_COLUMNS} FROM files
         WHERE norm_name = ?1 AND status = 'online'{}
         ORDER BY path
         LIMIT ?3 OFFSET ?4",
        volume_filter("files")
    )
}

fn prefix_range_sql() -> String {
    format!(
        "SELECT {HIT_COLUMNS} FROM files
         WHERE norm_name >= ?1 AND norm_name < ?1 || char(0x10FFFF)
           AND status = 'online'{}
         LIMIT ?3 OFFSET ?4",
        volume_filter("files")
    )
}

fn prefix_like_sql() -> String {
    format!(
        "SELECT {HIT_COLUMNS} FROM files INDEXED BY idx_files_norm_name
         WHERE norm_name LIKE ?1 AND status = 'online'{}
         LIMIT ?3 OFFSET ?4",
        volume_filter("files")
    )
}

fn fts_match_sql(rank: bool) -> String {
    let order = if rank { "ORDER BY rank" } else { "" };
    format!(
        "SELECT f.id, f.path, f.name, f.size, f.is_dir, f.mtime
         FROM {FTS_TABLE} JOIN files f ON f.rowid = {FTS_TABLE}.rowid
         WHERE {FTS_TABLE} MATCH ?1 AND f.status = 'online'{}
         {order}
         LIMIT ?3 OFFSET ?4",
        volume_filter("f")
    )
}

fn fts_like_sql() -> String {
    format!(
        "SELECT {HIT_COLUMNS} FROM files
         WHERE norm_name LIKE '%' || ?1 || '%' AND status = 'online'{}
         LIMIT ?3 OFFSET ?4",
        volume_filter("files")
    )
}

fn explain(
    conn: &Connection,
    sql: &str,
    query: &str,
    volume: Option<&str>,
    limit: usize,
    offset: usize,
) -> Result<Vec<String>> {
    let plan_sql = format!("EXPLAIN QUERY PLAN {sql}");
    let mut statement = conn.prepare(&plan_sql)?;
    let mapped = statement
        .query_map(params![query, volume, limit as i64, offset as i64], |row| {
            row.get::<_, String>(3)
        })?;
    Ok(mapped.collect::<std::result::Result<Vec<_>, _>>()?)
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE name = ?1",
        params![name],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn map_hit(row: &rusqlite::Row<'_>) -> rusqlite::Result<SearchHit> {
    Ok(SearchHit {
        id: row.get(0)?,
        path: row.get(1)?,
        name: row.get(2)?,
        size: row.get::<_, i64>(3)?.max(0) as u64,
        is_dir: row.get::<_, i64>(4)? != 0,
        mtime: row.get(5)?,
    })
}
