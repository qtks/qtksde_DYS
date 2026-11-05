mod dedupe;
mod fts;
mod http;
mod search;
mod treemap;

pub use dedupe::{
    find_duplicates, DuplicateFile, DuplicateGroup, DuplicateOptions, DuplicateReport, FunnelStage,
};
pub use fts::{ensure_fts, fts_status, rebuild_fts, FtsStatus, FTS_TABLE};
pub use http::{serve, HttpOptions};
pub use search::{search, PrefixMode, SearchHit, SearchKind, SearchOptions, SearchResult};
pub use treemap::{
    aggregate_dir_totals, child_rows, ensure_dir_totals, treemap_rows, AggregateReport, ChildRow,
    TreemapRow,
};

pub use walker_index::IndexError;

pub type Result<T> = std::result::Result<T, IndexError>;

/// Open a read-only-ish connection for query work: normal connection, but
/// `query_only` is enabled so a bug in a query path can never mutate the index.
pub fn open_query_connection(path: &std::path::Path) -> Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    conn.execute_batch(
        "PRAGMA query_only=ON;
         PRAGMA temp_store=MEMORY;
         PRAGMA cache_size=-32768;",
    )?;
    Ok(conn)
}

/// Open a connection that may write auxiliary tables (`dir_totals`, FTS).
pub fn open_write_connection(path: &std::path::Path) -> Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA temp_store=MEMORY;
         PRAGMA cache_size=-32768;",
    )?;
    Ok(conn)
}
