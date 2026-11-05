use std::collections::HashMap;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::Result;

pub(crate) fn now_iso8601() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0);
    // Timestamps here are informational; second granularity avoids pulling in
    // chrono just for the aggregate table.
    format!("{seconds}")
}

/// Shared with `walker-index` so a freshly created database already has the
/// table `/api/volumes` joins.
const DIR_TOTALS_SQL: &str = walker_index::DIR_TOTALS_SQL;

#[derive(Debug, Clone, Serialize)]
pub struct AggregateReport {
    pub directories: u64,
    pub files_scanned: u64,
    pub online_files: u64,
    pub bytes_indexed: u64,
    pub rows_written: u64,
    pub load_ms: u64,
    pub aggregate_ms: u64,
    pub write_ms: u64,
    pub total_ms: u64,
}

struct DirRecord {
    id: String,
    parent: Option<String>,
    path: String,
}

/// Materialize per-directory subtree totals.
///
/// Choice: a **summary table** (`dir_totals`) instead of a recursive CTE per
/// treemap request.
/// * recursive CTE per request costs O(subtree) per call; at the root that is
///   a full 1M-row traversal for every hover/zoom.
/// * the summary table costs one O(N) rebuild (single pass over `files` plus a
///   reverse path-order accumulation, no recursion), and treemap then costs
///   O(children) — a single `idx_files_parent` lookup plus a join on a small
///   WITHOUT ROWID table.
/// * update cost: a full rebuild is O(N) (~seconds at 1M rows). Incrementally
///   it could be O(changed × depth), but that would require the indexer to
///   emit per-file deltas; M2 keeps the rebuild explicit (`walker-query
///   aggregate`) so the scan path stays untouched.
pub fn aggregate_dir_totals(conn: &Connection) -> Result<AggregateReport> {
    let started = Instant::now();
    conn.execute_batch(DIR_TOTALS_SQL)?;

    let load_started = Instant::now();
    let dirs: Vec<DirRecord> = {
        let mut statement = conn.prepare("SELECT id, parent_id, path FROM files WHERE is_dir=1")?;
        let mapped = statement.query_map([], |row| {
            Ok(DirRecord {
                id: row.get(0)?,
                parent: row.get(1)?,
                path: row.get(2)?,
            })
        })?;
        mapped.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut index_of: HashMap<&str, usize> = HashMap::with_capacity(dirs.len());
    for (index, dir) in dirs.iter().enumerate() {
        index_of.insert(dir.id.as_str(), index);
    }
    let parents: Vec<Option<usize>> = dirs
        .iter()
        .map(|dir| {
            dir.parent
                .as_deref()
                .and_then(|parent| index_of.get(parent).copied())
        })
        .collect();
    let mut bytes = vec![0u64; dirs.len()];
    let mut file_count = vec![0u64; dirs.len()];
    let mut dir_count = vec![1u64; dirs.len()];
    let load_ms = load_started.elapsed().as_millis() as u64;

    let aggregate_started = Instant::now();
    let mut files_scanned = 0u64;
    let mut online_files = 0u64;
    {
        let mut statement =
            conn.prepare("SELECT parent_id, size, status FROM files WHERE is_dir=0")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            files_scanned += 1;
            let parent: Option<String> = row.get(0)?;
            let size: i64 = row.get(1)?;
            let status: String = row.get(2)?;
            if status != "online" {
                continue;
            }
            let Some(parent) = parent else { continue };
            let Some(index) = index_of.get(parent.as_str()).copied() else {
                continue;
            };
            online_files += 1;
            bytes[index] += size.max(0) as u64;
            file_count[index] += 1;
        }
    }

    // Parents always sort before their descendants because the materialized
    // path is a strict prefix ("/a" < "/a/b"), so a reverse pass accumulates
    // every subtree into its direct parent without recursion.
    let mut order: Vec<usize> = (0..dirs.len()).collect();
    order.sort_unstable_by(|left, right| dirs[*left].path.cmp(&dirs[*right].path));
    for index in order.into_iter().rev() {
        if let Some(parent) = parents[index] {
            bytes[parent] += bytes[index];
            file_count[parent] += file_count[index];
            dir_count[parent] += dir_count[index];
        }
    }
    let aggregate_ms = aggregate_started.elapsed().as_millis() as u64;

    let write_started = Instant::now();
    let transaction = conn.unchecked_transaction()?;
    transaction.execute("DELETE FROM dir_totals", [])?;
    let now = now_iso8601();
    let mut rows_written = 0u64;
    {
        let mut statement = transaction.prepare(
            "INSERT INTO dir_totals (dir_id, path, bytes, file_count, dir_count, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for (index, dir) in dirs.iter().enumerate() {
            statement.execute(params![
                dir.id,
                dir.path,
                bytes[index] as i64,
                file_count[index] as i64,
                dir_count[index] as i64,
                now
            ])?;
            rows_written += 1;
        }
    }
    transaction.execute(
        "INSERT INTO meta(key, value) VALUES ('aggregate_built_at', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![now],
    )?;
    // Row count at build time lets startup detect a scan that ran afterwards.
    let source_rows = dirs.len() as u64 + files_scanned;
    transaction.execute(
        "INSERT INTO meta(key, value) VALUES ('aggregate_source_rows', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![source_rows.to_string()],
    )?;
    transaction.commit()?;
    let write_ms = write_started.elapsed().as_millis() as u64;

    let bytes_indexed: u64 = dirs
        .iter()
        .enumerate()
        .filter(|(_, dir)| dir.parent.is_none())
        .map(|(index, _)| bytes[index])
        .sum();

    Ok(AggregateReport {
        directories: dirs.len() as u64,
        files_scanned,
        online_files,
        bytes_indexed,
        rows_written,
        load_ms,
        aggregate_ms,
        write_ms,
        total_ms: started.elapsed().as_millis() as u64,
    })
}

/// Rebuild `dir_totals` when it is missing or older than the current index.
///
/// `walker-index scan` leaves the summary table alone (see the note above), so
/// a user who follows the first-run instructions would otherwise see a volume
/// with `0 B / 0 files` and an empty treemap. The server calls this once at
/// startup: after a scan and restart the totals are current, and repeated
/// starts are a no-op because the indexed row count still matches.
pub fn ensure_dir_totals(conn: &Connection) -> Result<Option<AggregateReport>> {
    let entries: i64 = conn.query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))?;
    if entries == 0 {
        return Ok(None);
    }
    let totals: i64 = conn.query_row("SELECT COUNT(*) FROM dir_totals", [], |row| row.get(0))?;
    let built_rows: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key='aggregate_source_rows'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let current_rows = entries.to_string();
    if totals > 0 && built_rows.as_deref() == Some(current_rows.as_str()) {
        return Ok(None);
    }
    aggregate_dir_totals(conn).map(Some)
}

#[derive(Debug, Clone, Serialize)]
pub struct TreemapRow {
    pub id: String,
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    /// Subtree bytes for directories, own size for files.
    pub bytes: u64,
    pub file_count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TreemapData {
    pub id: String,
    pub path: String,
    pub is_dir: bool,
    pub bytes: u64,
    pub file_count: u64,
    pub dir_count: u64,
    pub children: Vec<TreemapRow>,
    pub took_us: u64,
    /// True when the children list was truncated by `limit`.
    pub truncated: bool,
}

pub fn treemap_rows(conn: &Connection, path: Option<&str>, limit: usize) -> Result<TreemapData> {
    let started = Instant::now();
    let limit = limit.clamp(1, 1000);
    let target = match path {
        Some(path) => resolve_dir_by_path(conn, path)?,
        None => {
            // No path: return the volume roots as a synthetic parent list.
            let volumes: Vec<String> = {
                let mut statement =
                    conn.prepare("SELECT mount_path FROM volumes WHERE status='online'")?;
                let mapped = statement.query_map([], |row| row.get::<_, String>(0))?;
                mapped.collect::<std::result::Result<Vec<_>, _>>()?
            };
            let mut children = Vec::new();
            let mut bytes = 0u64;
            let mut file_count = 0u64;
            let mut dir_count = 0u64;
            for mount_path in &volumes {
                let Some(root) = find_dir_blob(conn, mount_path)? else {
                    continue;
                };
                bytes += root.bytes;
                file_count += root.file_count;
                dir_count += root.dir_count;
                children.push(TreemapRow {
                    id: root.id,
                    name: mount_path.clone(),
                    path: mount_path.clone(),
                    is_dir: true,
                    bytes: root.bytes,
                    file_count: root.file_count,
                });
            }
            children.sort_by_key(|child| std::cmp::Reverse(child.bytes));
            return Ok(TreemapData {
                id: String::new(),
                path: "/".to_string(),
                is_dir: true,
                bytes,
                file_count,
                dir_count,
                children,
                took_us: started.elapsed().as_micros() as u64,
                truncated: false,
            });
        }
    };

    let children = children_of(conn, &target, limit)?;
    let truncated = children.len() >= limit;
    Ok(TreemapData {
        id: target.id,
        path: target.path,
        is_dir: true,
        bytes: target.bytes,
        file_count: target.file_count,
        dir_count: target.dir_count,
        children,
        took_us: started.elapsed().as_micros() as u64,
        truncated,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct ChildRow {
    pub id: String,
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub bytes: u64,
    pub size: u64,
    pub file_count: u64,
    pub mtime: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChildPage {
    pub path: String,
    pub parent_id: String,
    pub children: Vec<ChildRow>,
    pub truncated: bool,
    pub took_us: u64,
}

/// Children of one directory, used by the tree's lazy loading and by treemap.
pub fn child_rows(
    conn: &Connection,
    path: &str,
    limit: usize,
    offset: usize,
    sort: &str,
) -> Result<ChildPage> {
    let started = Instant::now();
    let target = resolve_dir_by_path(conn, path)?;
    let limit = limit.clamp(1, 500_000);
    let order = if sort == "name" {
        "c.name ASC"
    } else {
        "bytes DESC, c.name ASC"
    };
    let sql = format!(
        "SELECT c.id, c.name, c.path, c.is_dir, c.size, c.mtime,
                CASE WHEN c.is_dir=1 THEN COALESCE(t.bytes,0) ELSE c.size END AS bytes,
                CASE WHEN c.is_dir=1 THEN COALESCE(t.file_count,1) ELSE 1 END AS file_count
         FROM files c
         LEFT JOIN dir_totals t ON t.dir_id = c.id
         WHERE c.parent_id = ?1 AND c.status = 'online'
         ORDER BY {order}
         LIMIT ?2 OFFSET ?3"
    );
    let mut statement = conn.prepare(&sql)?;
    let mapped = statement.query_map(params![target.id, limit as i64, offset as i64], |row| {
        Ok(ChildRow {
            id: row.get(0)?,
            name: row.get(1)?,
            path: row.get(2)?,
            is_dir: row.get::<_, i64>(3)? != 0,
            bytes: row.get::<_, i64>(6)?.max(0) as u64,
            size: row.get::<_, i64>(4)?.max(0) as u64,
            file_count: row.get::<_, i64>(7)?.max(0) as u64,
            mtime: row.get(5)?,
        })
    })?;
    let children: Vec<ChildRow> = mapped.collect::<std::result::Result<Vec<_>, _>>()?;
    let truncated = children.len() >= limit;
    Ok(ChildPage {
        path: target.path,
        parent_id: target.id,
        children,
        truncated,
        took_us: started.elapsed().as_micros() as u64,
    })
}

struct DirTotals {
    id: String,
    path: String,
    bytes: u64,
    file_count: u64,
    dir_count: u64,
}

fn find_dir_blob(conn: &Connection, path: &str) -> Result<Option<DirTotals>> {
    let found: Option<(String, String, i64, i64, i64)> = conn
        .query_row(
            "SELECT c.id, c.path, COALESCE(t.bytes,0), COALESCE(t.file_count,0),
                    COALESCE(t.dir_count,0)
             FROM files c LEFT JOIN dir_totals t ON t.dir_id=c.id
             WHERE c.path = ?1 AND c.is_dir = 1
             LIMIT 1",
            params![path],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .ok();
    Ok(
        found.map(|(id, path, bytes, file_count, dir_count)| DirTotals {
            id,
            path,
            bytes: bytes.max(0) as u64,
            file_count: file_count.max(0) as u64,
            dir_count: dir_count.max(0) as u64,
        }),
    )
}

fn resolve_dir_by_path(conn: &Connection, path: &str) -> Result<DirTotals> {
    find_dir_blob(conn, path)?
        .ok_or_else(|| crate::IndexError::Config(format!("directory not indexed: {path}")))
}

fn children_of(conn: &Connection, target: &DirTotals, limit: usize) -> Result<Vec<TreemapRow>> {
    let mut statement = conn.prepare(
        "SELECT c.id, c.name, c.path, c.is_dir,
                CASE WHEN c.is_dir=1 THEN COALESCE(t.bytes,0) ELSE c.size END AS bytes,
                CASE WHEN c.is_dir=1 THEN COALESCE(t.file_count,1) ELSE 1 END AS file_count
         FROM files c
         LEFT JOIN dir_totals t ON t.dir_id = c.id
         WHERE c.parent_id = ?1 AND c.status = 'online'
         ORDER BY bytes DESC, c.name ASC
         LIMIT ?2",
    )?;
    let mapped = statement.query_map(params![target.id, limit as i64], |row| {
        Ok(TreemapRow {
            id: row.get(0)?,
            name: row.get(1)?,
            path: row.get(2)?,
            is_dir: row.get::<_, i64>(3)? != 0,
            bytes: row.get::<_, i64>(4)?.max(0) as u64,
            file_count: row.get::<_, i64>(5)?.max(0) as u64,
        })
    })?;
    Ok(mapped.collect::<std::result::Result<Vec<_>, _>>()?)
}
