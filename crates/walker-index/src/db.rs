use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension, Transaction};
use uuid::Uuid;

use walker_core::{extension_of, normalize_name, FileEntry};

use crate::error::IndexError;
use crate::model::{
    ScanSession, SessionPhase, SessionStatus, Volume, VolumeStatus, INDEX_SCHEMA_VERSION,
};
use crate::timeutil::now_iso8601;
use crate::work::WorkEntry;

pub const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS volumes (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    mount_path   TEXT NOT NULL,
    device       INTEGER,
    status       TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_volumes_mount_path ON volumes(mount_path);

CREATE TABLE IF NOT EXISTS files (
    id                     TEXT PRIMARY KEY,
    volume_id              TEXT NOT NULL REFERENCES volumes(id) ON DELETE CASCADE,
    parent_id              TEXT REFERENCES files(id) ON DELETE CASCADE,
    name                   TEXT NOT NULL,
    norm_name              TEXT NOT NULL,
    path                   TEXT NOT NULL,
    is_dir                 INTEGER NOT NULL,
    ext                    TEXT,
    size                   INTEGER NOT NULL DEFAULT 0,
    mtime                  INTEGER NOT NULL DEFAULT 0,
    inode                  INTEGER NOT NULL DEFAULT 0,
    file_type              TEXT NOT NULL,
    content_hash           TEXT,
    project_id             TEXT,
    summary                TEXT,
    embedding              BLOB,
    scan_batch             INTEGER,
    status                 TEXT NOT NULL DEFAULT 'online',
    status_before_offline  TEXT,
    deleted_at             TEXT,
    created_at             TEXT NOT NULL,
    updated_at             TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_files_path ON files(volume_id, path);
CREATE INDEX IF NOT EXISTS idx_files_parent ON files(parent_id);
CREATE INDEX IF NOT EXISTS idx_files_hash ON files(content_hash) WHERE content_hash IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_files_norm_name ON files(norm_name);

CREATE TABLE IF NOT EXISTS scan_sessions (
    id                 TEXT PRIMARY KEY,
    volume_id          TEXT NOT NULL REFERENCES volumes(id) ON DELETE CASCADE,
    root_path          TEXT NOT NULL,
    phase              TEXT NOT NULL,
    status             TEXT NOT NULL,
    work_file          TEXT,
    dirs_file          TEXT,
    work_file_offset   INTEGER NOT NULL DEFAULT 0,
    total_entries      INTEGER NOT NULL DEFAULT 0,
    completed_batches  INTEGER NOT NULL DEFAULT 0,
    batch_size         INTEGER NOT NULL,
    cursor_path        TEXT,
    started_at         TEXT NOT NULL,
    updated_at         TEXT NOT NULL,
    finished_at        TEXT
);
CREATE INDEX IF NOT EXISTS idx_scan_sessions_volume ON scan_sessions(volume_id, status);
"#;

/// Per-directory subtree totals read by the treemap and volume summary.
///
/// These rows are materialized by `walker-query aggregate`, but the table has
/// to exist from the first launch: `/api/volumes` joins it, so a brand-new
/// index would otherwise answer 500 with "no such table: dir_totals" before
/// anyone ran the aggregate step.
pub const DIR_TOTALS_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS dir_totals (
    dir_id     TEXT PRIMARY KEY,
    path       TEXT NOT NULL,
    bytes      INTEGER NOT NULL,
    file_count INTEGER NOT NULL,
    dir_count  INTEGER NOT NULL,
    updated_at TEXT NOT NULL
) WITHOUT ROWID;
"#;

pub struct IndexDb {
    conn: Connection,
}

impl IndexDb {
    pub fn open(path: &Path) -> Result<Self, IndexError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;

        let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(IndexError::Config(format!(
                "SQLite refused WAL mode (journal_mode={mode})"
            )));
        }
        conn.execute_batch(
            "PRAGMA synchronous=NORMAL;\n\
             PRAGMA foreign_keys=ON;\n\
             PRAGMA temp_store=MEMORY;\n\
             PRAGMA cache_size=-131072;\n\
             PRAGMA wal_autocheckpoint=0;",
        )?;

        let db = Self { conn };
        db.init_schema()?;
        Ok(db)
    }

    pub fn open_in_memory() -> Result<Self, IndexError> {
        let conn = Connection::open_in_memory()?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA foreign_keys=ON;\n\
             PRAGMA temp_store=MEMORY;",
        )?;
        let db = Self { conn };
        db.init_schema()?;
        Ok(db)
    }

    fn init_schema(&self) -> Result<(), IndexError> {
        self.conn.execute_batch(SCHEMA_SQL)?;
        self.conn.execute_batch(DIR_TOTALS_SQL)?;
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES ('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![INDEX_SCHEMA_VERSION],
        )?;
        Ok(())
    }

    /// Read-only access for tests and the CLI reporting helpers.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Fold the WAL back into the main database file. Bulk loading disables
    /// auto-checkpoints so page writes happen once, in one sequential pass.
    pub fn checkpoint(&mut self) -> Result<(), IndexError> {
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }

    /// Number of indexed rows for a volume (any status). A fresh volume has
    /// zero rows, which is when we can skip maintaining secondary indexes
    /// during the bulk insert and build them once at the end instead.
    pub fn volume_row_count(&self, volume_id: &str) -> Result<u64, IndexError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM files WHERE volume_id = ?1",
            params![volume_id],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    /// Secondary indexes that are only needed for queries, not for the bulk
    /// insert path. Dropping them up front turns 1M random index inserts into
    /// one external sort per index. Crash-safe: `open()` re-creates anything
    /// that is missing through `SCHEMA_SQL`.
    pub fn drop_secondary_indexes(&self) -> Result<(), IndexError> {
        self.conn.execute_batch(
            "DROP INDEX IF EXISTS idx_files_parent;
             DROP INDEX IF EXISTS idx_files_norm_name;",
        )?;
        Ok(())
    }

    pub fn rebuild_secondary_indexes(&self) -> Result<(), IndexError> {
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_files_parent ON files(parent_id);
             CREATE INDEX IF NOT EXISTS idx_files_norm_name ON files(norm_name);",
        )?;
        Ok(())
    }

    pub fn journal_mode(&self) -> Result<String, IndexError> {
        Ok(self
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))?)
    }

    pub fn total_changes(&self) -> u64 {
        self.conn.total_changes()
    }

    // ---------------------------------------------------------------- volumes

    pub fn attach_volume(
        &mut self,
        mount_path: &str,
        device: Option<u64>,
    ) -> Result<(Volume, bool), IndexError> {
        let now = now_iso8601();
        let existing = self.volume_by_mount_path(mount_path)?;
        match existing {
            Some(volume) => {
                let was_offline = volume.status == VolumeStatus::Offline;
                self.conn.execute(
                    "UPDATE volumes SET status='online', device=?2, last_seen_at=?3, updated_at=?3
                     WHERE id=?1",
                    params![volume.id, device.map(|value| value as i64), now.as_str()],
                )?;
                let updated = Volume {
                    status: VolumeStatus::Online,
                    device,
                    last_seen_at: now.clone(),
                    updated_at: now,
                    ..volume
                };
                Ok((updated, was_offline))
            }
            None => {
                let name = Path::new(mount_path)
                    .file_name()
                    .map(|value| value.to_string_lossy().into_owned())
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| mount_path.to_string());
                let id = Uuid::new_v4().to_string();
                self.conn.execute(
                    "INSERT INTO volumes
                        (id, name, mount_path, device, status, last_seen_at, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, 'online', ?5, ?5, ?5)",
                    params![
                        id,
                        name,
                        mount_path,
                        device.map(|value| value as i64),
                        now.as_str()
                    ],
                )?;
                Ok((
                    Volume {
                        id,
                        name,
                        mount_path: PathBuf::from(mount_path),
                        device,
                        status: VolumeStatus::Online,
                        last_seen_at: now.clone(),
                        created_at: now.clone(),
                        updated_at: now,
                    },
                    false,
                ))
            }
        }
    }

    pub fn volume_by_mount_path(&self, mount_path: &str) -> Result<Option<Volume>, IndexError> {
        let mut statement = self.conn.prepare(
            "SELECT id, name, mount_path, device, status, last_seen_at, created_at, updated_at
             FROM volumes WHERE mount_path=?1",
        )?;
        let row = statement
            .query_row(params![mount_path], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                ))
            })
            .optional()?;
        Ok(row.map(|row| Volume {
            id: row.0,
            name: row.1,
            mount_path: PathBuf::from(row.2),
            device: row.3.map(|value| value as u64),
            status: VolumeStatus::parse(&row.4).unwrap_or(VolumeStatus::Offline),
            last_seen_at: row.5,
            created_at: row.6,
            updated_at: row.7,
        }))
    }

    pub fn volumes(&self) -> Result<Vec<Volume>, IndexError> {
        let mut statement = self.conn.prepare(
            "SELECT id, name, mount_path, device, status, last_seen_at, created_at, updated_at
             FROM volumes ORDER BY mount_path",
        )?;
        let mapped = statement.query_map([], |row| {
            Ok(Volume {
                id: row.get(0)?,
                name: row.get(1)?,
                mount_path: PathBuf::from(row.get::<_, String>(2)?),
                device: row.get::<_, Option<i64>>(3)?.map(|value| value as u64),
                status: VolumeStatus::parse(&row.get::<_, String>(4)?)
                    .unwrap_or(VolumeStatus::Offline),
                last_seen_at: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
            })
        })?;
        Ok(mapped.collect::<Result<Vec<_>, _>>()?)
    }

    /// Unplugging a volume moves its rows to `offline` while preserving their
    /// scan-derived status in `status_before_offline`; rows are never deleted.
    pub fn mark_volume_offline(&mut self, volume_id: &str, now: &str) -> Result<u64, IndexError> {
        let transaction = self.conn.transaction()?;
        let affected = transaction.execute(
            "UPDATE files SET status_before_offline = status, status = 'offline', updated_at = ?2
             WHERE volume_id = ?1 AND status <> 'offline'",
            params![volume_id, now],
        )?;
        transaction.execute(
            "UPDATE volumes SET status = 'offline', updated_at = ?2 WHERE id = ?1",
            params![volume_id, now],
        )?;
        transaction.commit()?;
        Ok(affected as u64)
    }

    /// Re-attaching a volume restores every `offline` row to the status it had
    /// before being unplugged. Returns `true` if the volume was offline.
    pub fn restore_volume(
        &mut self,
        volume_id: &str,
        now: &str,
    ) -> Result<(bool, u64), IndexError> {
        let transaction = self.conn.transaction()?;
        let status: Option<String> = transaction
            .query_row(
                "SELECT status FROM volumes WHERE id=?1",
                params![volume_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(status) = status else {
            return Err(IndexError::VolumeNotFound(volume_id.to_string()));
        };
        if status != "offline" {
            transaction.commit()?;
            return Ok((false, 0));
        }
        let restored = transaction.execute(
            "UPDATE files
             SET status = COALESCE(status_before_offline, 'online'),
                 status_before_offline = NULL,
                 updated_at = ?2
             WHERE volume_id = ?1 AND status = 'offline'",
            params![volume_id, now],
        )?;
        transaction.execute(
            "UPDATE volumes SET status = 'online', updated_at = ?2 WHERE id = ?1",
            params![volume_id, now],
        )?;
        transaction.commit()?;
        Ok((true, restored as u64))
    }

    /// Restores file rows that were moved to `offline`, without touching the
    /// volume row. Used when a scan re-attaches an already-`online` volume.
    pub fn restore_offline_files(&self, volume_id: &str, now: &str) -> Result<u64, IndexError> {
        let restored = self.conn.execute(
            "UPDATE files
             SET status = COALESCE(status_before_offline, 'online'),
                 status_before_offline = NULL,
                 updated_at = ?2
             WHERE volume_id = ?1 AND status = 'offline'",
            params![volume_id, now],
        )?;
        Ok(restored as u64)
    }

    pub fn directory_ids(&self, volume_id: &str) -> Result<Vec<(String, String)>, IndexError> {
        let mut statement = self
            .conn
            .prepare("SELECT path, id FROM files WHERE volume_id=?1 AND is_dir=1")?;
        let mapped =
            statement.query_map(params![volume_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(mapped.collect::<Result<Vec<_>, _>>()?)
    }

    // --------------------------------------------------------------- sessions

    pub fn create_session(
        &self,
        volume_id: &str,
        root_path: &str,
        batch_size: usize,
    ) -> Result<ScanSession, IndexError> {
        let now = now_iso8601();
        let id = Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO scan_sessions
                (id, volume_id, root_path, phase, status, batch_size, started_at, updated_at)
             VALUES (?1, ?2, ?3, 'collecting', 'running', ?4, ?5, ?5)",
            params![id, volume_id, root_path, batch_size as i64, now.as_str()],
        )?;
        Ok(ScanSession {
            id,
            volume_id: volume_id.to_string(),
            root_path: root_path.to_string(),
            phase: SessionPhase::Collecting,
            status: SessionStatus::Running,
            work_file: None,
            dirs_file: None,
            work_file_offset: 0,
            total_entries: 0,
            completed_batches: 0,
            batch_size: batch_size as u64,
            cursor_path: None,
            started_at: now.clone(),
            updated_at: now,
            finished_at: None,
        })
    }

    pub fn latest_running_session(
        &self,
        volume_id: &str,
    ) -> Result<Option<ScanSession>, IndexError> {
        let mut statement = self.conn.prepare(
            "SELECT id, volume_id, root_path, phase, status, work_file, dirs_file,
                    work_file_offset, total_entries, completed_batches, batch_size,
                    cursor_path, started_at, updated_at, finished_at
             FROM scan_sessions
             WHERE volume_id = ?1 AND status = 'running'
             ORDER BY started_at DESC, rowid DESC
             LIMIT 1",
        )?;
        let row = statement
            .query_row(params![volume_id], map_session_row)
            .optional()?;
        Ok(row)
    }

    pub fn abort_running_sessions(&self, volume_id: &str) -> Result<(), IndexError> {
        self.conn.execute(
            "UPDATE scan_sessions SET status='aborted', updated_at=?2
             WHERE volume_id=?1 AND status='running'",
            params![volume_id, now_iso8601()],
        )?;
        Ok(())
    }

    pub fn reset_session_collecting(&self, session_id: &str) -> Result<(), IndexError> {
        self.conn.execute(
            "UPDATE scan_sessions
             SET phase='collecting', status='running', work_file=NULL, dirs_file=NULL,
                 work_file_offset=0, total_entries=0, completed_batches=0, cursor_path=NULL,
                 updated_at=?2, finished_at=NULL
             WHERE id=?1",
            params![session_id, now_iso8601()],
        )?;
        Ok(())
    }

    pub fn mark_session_writing(
        &self,
        session_id: &str,
        total_entries: u64,
        work_file: &str,
        dirs_file: &str,
    ) -> Result<(), IndexError> {
        self.conn.execute(
            "UPDATE scan_sessions
             SET phase='writing', work_file=?2, dirs_file=?3, total_entries=?4,
                 work_file_offset=0, completed_batches=0, cursor_path=NULL, updated_at=?5
             WHERE id=?1",
            params![
                session_id,
                work_file,
                dirs_file,
                total_entries as i64,
                now_iso8601()
            ],
        )?;
        Ok(())
    }

    pub fn finish_session(&self, session_id: &str, total_entries: u64) -> Result<(), IndexError> {
        let now = now_iso8601();
        self.conn.execute(
            "UPDATE scan_sessions
             SET phase='done', status='completed', total_entries=?2, finished_at=?3, updated_at=?3
             WHERE id=?1",
            params![session_id, total_entries as i64, now],
        )?;
        Ok(())
    }

    // --------------------------------------------------------------- batches

    pub(crate) fn apply_batch(
        &mut self,
        entries: &[WorkEntry],
        context: &mut BatchContext<'_>,
    ) -> Result<BatchOutcome, IndexError> {
        let start_path = context.cursor.as_ref().cloned().unwrap_or_default();
        let end_path = entries
            .last()
            .map(|entry| entry.path.clone())
            .unwrap_or_default();

        let transaction = self.conn.transaction()?;
        let rows = existing_rows(&transaction, context.volume_id, &start_path, &end_path)?;

        let mut outcome = BatchOutcome::default();
        let mut left = 0usize;
        let mut right = 0usize;
        let mut applied = 0u64;

        while left < entries.len() || right < rows.len() {
            let ordering = match (entries.get(left), rows.get(right)) {
                (Some(entry), Some(row)) => Some(entry.path.cmp(&row.path)),
                (Some(_), None) => Some(std::cmp::Ordering::Less),
                (None, Some(_)) => Some(std::cmp::Ordering::Greater),
                (None, None) => None,
            };
            match ordering {
                Some(std::cmp::Ordering::Less) => {
                    let entry = &entries[left];
                    let parent_id = resolve_parent(
                        &transaction,
                        context.volume_id,
                        context.root_path,
                        &entry.path,
                        context.dir_ids,
                    )?;
                    insert_row(&transaction, context, entry, parent_id.as_deref())?;
                    outcome.inserted += 1;
                    outcome.rows_written += 1;
                    left += 1;
                    applied += 1;
                    check_injected_failure(context, applied)?;
                }
                Some(std::cmp::Ordering::Equal) => {
                    let entry = &entries[left];
                    let row = &rows[right];
                    let parent_id = if row.is_dir {
                        context.dir_ids.insert(row.path.clone(), row.id.clone());
                        row.parent_id.clone()
                    } else {
                        row.parent_id.clone()
                    };
                    let same = row.inode == entry.inode
                        && row.mtime == entry.mtime
                        && row.size == entry.size
                        && row.is_dir == entry.is_dir()
                        && row.file_type == entry.kind.as_str()
                        && row.status == "online";
                    if same {
                        outcome.unchanged += 1;
                    } else {
                        update_row(&transaction, context, entry, row, parent_id.as_deref())?;
                        outcome.updated += 1;
                        outcome.rows_written += 1;
                        applied += 1;
                        check_injected_failure(context, applied)?;
                    }
                    left += 1;
                    right += 1;
                }
                Some(std::cmp::Ordering::Greater) => {
                    let row = &rows[right];
                    if missing_row(&transaction, context, row)? {
                        outcome.marked_missing += 1;
                        outcome.rows_written += 1;
                        applied += 1;
                        check_injected_failure(context, applied)?;
                    }
                    if row.is_dir {
                        let cascaded = cascade_missing(&transaction, context, &row.path)?;
                        outcome.cascaded_missing += cascaded;
                        outcome.rows_written += cascaded;
                    }
                    right += 1;
                }
                None => break,
            }
        }

        let cursor = entries.last().map(|entry| entry.path.clone());
        transaction.execute(
            "UPDATE scan_sessions
             SET phase='writing', completed_batches=?2, work_file_offset=?3, cursor_path=?4,
                 updated_at=?5
             WHERE id=?1",
            params![
                context.session_id,
                context.batch_index as i64,
                context.new_offset as i64,
                cursor,
                context.now
            ],
        )?;
        transaction.commit()?;
        *context.cursor = cursor;
        Ok(outcome)
    }

    /// After all entry batches, any remaining DB row for the volume was not
    /// seen. Those whose parent directory was fully listed are marked missing.
    pub(crate) fn cleanup_chunk(
        &mut self,
        volume_id: &str,
        root_path: &str,
        session_id: &str,
        dirs_listed: &HashSet<String>,
        cursor: &mut Option<String>,
        now: &str,
    ) -> Result<CleanupOutcome, IndexError> {
        let transaction = self.conn.transaction()?;
        let rows = {
            let start = cursor.clone().unwrap_or_default();
            let mut statement = transaction.prepare(
                "SELECT id, path, parent_id, is_dir, inode, mtime, size, file_type, status
                 FROM files
                 WHERE volume_id = ?1 AND path > ?2
                 ORDER BY path
                 LIMIT 5000",
            )?;
            let mapped = statement.query_map(params![volume_id, start], map_existing_row)?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };
        let mut outcome = CleanupOutcome::default();
        if rows.is_empty() {
            transaction.commit()?;
            return Ok(outcome);
        }
        for row in &rows {
            if !is_parent_listed(row, root_path, dirs_listed) {
                continue;
            }
            if mark_missing(&transaction, row, now)? {
                outcome.marked += 1;
            }
            if row.is_dir {
                outcome.cascaded += cascade_missing_raw(&transaction, volume_id, &row.path, now)?;
            }
        }
        let last = rows.last().map(|row| row.path.clone());
        transaction.execute(
            "UPDATE scan_sessions SET work_file_offset = work_file_offset, cursor_path = ?2,
                 updated_at = ?3 WHERE id = ?1",
            params![session_id, last, now],
        )?;
        transaction.commit()?;
        *cursor = last;
        outcome.rows = rows.len() as u64;
        Ok(outcome)
    }

    // ------------------------------------------------------------ reporting

    pub fn file_counts_by_type(&self, volume_id: &str) -> Result<Vec<(String, u64)>, IndexError> {
        let mut statement = self.conn.prepare(
            "SELECT file_type, COUNT(*) FROM files WHERE volume_id=?1 GROUP BY file_type ORDER BY file_type",
        )?;
        let mapped = statement.query_map(params![volume_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
        })?;
        Ok(mapped.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn file_counts_by_status(&self, volume_id: &str) -> Result<Vec<(String, u64)>, IndexError> {
        let mut statement = self.conn.prepare(
            "SELECT status, COUNT(*) FROM files WHERE volume_id=?1 GROUP BY status ORDER BY status",
        )?;
        let mapped = statement.query_map(params![volume_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
        })?;
        Ok(mapped.collect::<Result<Vec<_>, _>>()?)
    }
}

fn map_session_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ScanSession> {
    Ok(ScanSession {
        id: row.get(0)?,
        volume_id: row.get(1)?,
        root_path: row.get(2)?,
        phase: SessionPhase::parse(&row.get::<_, String>(3)?).unwrap_or(SessionPhase::Collecting),
        status: match row.get::<_, String>(4)?.as_str() {
            "completed" => SessionStatus::Completed,
            "aborted" => SessionStatus::Aborted,
            _ => SessionStatus::Running,
        },
        work_file: row.get::<_, Option<String>>(5)?.map(PathBuf::from),
        dirs_file: row.get::<_, Option<String>>(6)?.map(PathBuf::from),
        work_file_offset: row.get::<_, i64>(7)? as u64,
        total_entries: row.get::<_, i64>(8)? as u64,
        completed_batches: row.get::<_, i64>(9)? as u64,
        batch_size: row.get::<_, i64>(10)? as u64,
        cursor_path: row.get(11)?,
        started_at: row.get(12)?,
        updated_at: row.get(13)?,
        finished_at: row.get(14)?,
    })
}

struct ExistingRow {
    id: String,
    path: String,
    parent_id: Option<String>,
    is_dir: bool,
    inode: u64,
    mtime: i64,
    size: u64,
    file_type: String,
    status: String,
}

fn map_existing_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ExistingRow> {
    Ok(ExistingRow {
        id: row.get(0)?,
        path: row.get(1)?,
        parent_id: row.get(2)?,
        is_dir: row.get::<_, i64>(3)? != 0,
        inode: row.get::<_, i64>(4)? as u64,
        mtime: row.get(5)?,
        size: row.get::<_, i64>(6)? as u64,
        file_type: row.get(7)?,
        status: row.get(8)?,
    })
}

fn existing_rows(
    transaction: &Transaction<'_>,
    volume_id: &str,
    start_path: &str,
    end_path: &str,
) -> Result<Vec<ExistingRow>, IndexError> {
    let mut statement = transaction.prepare(
        "SELECT id, path, parent_id, is_dir, inode, mtime, size, file_type, status
         FROM files
         WHERE volume_id = ?1 AND path > ?2 AND path <= ?3
         ORDER BY path",
    )?;
    let mapped = statement.query_map(params![volume_id, start_path, end_path], map_existing_row)?;
    Ok(mapped.collect::<Result<Vec<_>, _>>()?)
}

pub(crate) struct BatchContext<'a> {
    pub volume_id: &'a str,
    pub root_path: &'a str,
    pub session_id: &'a str,
    pub dirs_listed: &'a HashSet<String>,
    pub dir_ids: &'a mut HashMap<String, String>,
    pub cursor: &'a mut Option<String>,
    pub batch_index: u64,
    pub new_offset: u64,
    pub now: &'a str,
    pub fail_after_rows: Option<u64>,
}

#[derive(Debug, Default)]
pub(crate) struct BatchOutcome {
    pub inserted: u64,
    pub updated: u64,
    pub unchanged: u64,
    pub marked_missing: u64,
    pub cascaded_missing: u64,
    pub rows_written: u64,
}

#[derive(Debug, Default)]
pub(crate) struct CleanupOutcome {
    pub rows: u64,
    pub marked: u64,
    pub cascaded: u64,
}

fn check_injected_failure(context: &BatchContext<'_>, applied: u64) -> Result<(), IndexError> {
    let Some(limit) = context.fail_after_rows else {
        return Ok(());
    };
    if context.batch_index != 1 || applied < limit {
        return Ok(());
    }
    Err(IndexError::InjectedFailure { rows: applied })
}

fn resolve_parent(
    transaction: &Transaction<'_>,
    volume_id: &str,
    root_path: &str,
    path: &str,
    dir_ids: &mut HashMap<String, String>,
) -> Result<Option<String>, IndexError> {
    if path == root_path {
        return Ok(None);
    }
    let Some(parent_path) = Path::new(path)
        .parent()
        .map(|parent| parent.to_string_lossy().into_owned())
    else {
        return Ok(None);
    };
    if let Some(id) = dir_ids.get(&parent_path) {
        return Ok(Some(id.clone()));
    }
    let found: Option<String> = transaction
        .query_row(
            "SELECT id FROM files WHERE volume_id=?1 AND path=?2",
            params![volume_id, parent_path],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(id) = &found {
        dir_ids.insert(parent_path, id.clone());
    }
    Ok(found)
}

fn names_for(path: &str) -> (String, String, Option<String>) {
    let name = Path::new(path)
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let norm_name = normalize_name(&name);
    let extension = extension_of(&norm_name);
    (name, norm_name, extension)
}

fn insert_row(
    transaction: &Transaction<'_>,
    context: &mut BatchContext<'_>,
    entry: &WorkEntry,
    parent_id: Option<&str>,
) -> Result<(), IndexError> {
    let id = Uuid::now_v7().to_string();
    let (name, norm_name, extension) = names_for(&entry.path);
    let mut statement = transaction.prepare_cached(
        "INSERT INTO files
            (id, volume_id, parent_id, name, norm_name, path, is_dir, ext, size, mtime, inode,
             file_type, content_hash, project_id, summary, embedding, scan_batch, status,
             status_before_offline, deleted_at, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                 NULL, NULL, NULL, NULL, ?13, 'online', NULL, NULL, ?14, ?14)",
    )?;
    statement.execute(params![
        id,
        context.volume_id,
        parent_id,
        name,
        norm_name,
        entry.path,
        i64::from(entry.is_dir()),
        extension,
        entry.size as i64,
        entry.mtime,
        entry.inode as i64,
        entry.kind.as_str(),
        context.batch_index as i64,
        context.now
    ])?;
    if entry.is_dir() {
        context.dir_ids.insert(entry.path.clone(), id);
    }
    Ok(())
}

fn update_row(
    transaction: &Transaction<'_>,
    context: &mut BatchContext<'_>,
    entry: &WorkEntry,
    row: &ExistingRow,
    parent_id: Option<&str>,
) -> Result<(), IndexError> {
    let (name, norm_name, extension) = names_for(&entry.path);
    let mut statement = transaction.prepare_cached(
        "UPDATE files
         SET parent_id=?2, name=?3, norm_name=?4, path=?5, is_dir=?6, ext=?7, size=?8,
             mtime=?9, inode=?10, file_type=?11, status='online', status_before_offline=NULL,
             deleted_at=NULL, scan_batch=?12, updated_at=?13
         WHERE id=?1",
    )?;
    statement.execute(params![
        row.id,
        parent_id,
        name,
        norm_name,
        entry.path,
        i64::from(entry.is_dir()),
        extension,
        entry.size as i64,
        entry.mtime,
        entry.inode as i64,
        entry.kind.as_str(),
        context.batch_index as i64,
        context.now
    ])?;
    if row.is_dir {
        context.dir_ids.insert(row.path.clone(), row.id.clone());
    }
    Ok(())
}

fn missing_row(
    transaction: &Transaction<'_>,
    context: &BatchContext<'_>,
    row: &ExistingRow,
) -> Result<bool, IndexError> {
    if !is_parent_listed(row, context.root_path, context.dirs_listed) {
        return Ok(false);
    }
    mark_missing(transaction, row, context.now)
}

fn mark_missing(
    transaction: &Transaction<'_>,
    row: &ExistingRow,
    now: &str,
) -> Result<bool, IndexError> {
    if row.status == "missing" {
        return Ok(false);
    }
    let affected = transaction.execute(
        "UPDATE files SET status='missing', deleted_at=?2, updated_at=?2 WHERE id=?1 AND status<>'missing'",
        params![row.id, now],
    )?;
    Ok(affected > 0)
}

fn is_parent_listed(row: &ExistingRow, root_path: &str, dirs_listed: &HashSet<String>) -> bool {
    if row.path == root_path {
        return false;
    }
    let Some(parent) = Path::new(&row.path)
        .parent()
        .map(|value| value.to_string_lossy().into_owned())
    else {
        return false;
    };
    dirs_listed.contains(&parent)
}

fn cascade_missing(
    transaction: &Transaction<'_>,
    context: &BatchContext<'_>,
    directory_path: &str,
) -> Result<u64, IndexError> {
    cascade_missing_raw(transaction, context.volume_id, directory_path, context.now)
}

fn cascade_missing_raw(
    transaction: &Transaction<'_>,
    volume_id: &str,
    directory_path: &str,
    now: &str,
) -> Result<u64, IndexError> {
    let lower = format!("{directory_path}/");
    let upper = format!("{directory_path}0");
    let affected = transaction.execute(
        "UPDATE files
         SET status='missing', deleted_at=?2, updated_at=?2
         WHERE volume_id=?1 AND path > ?3 AND path < ?4 AND status='online'",
        params![volume_id, now, lower, upper],
    )?;
    Ok(affected as u64)
}

/// Convert scanned entries into the compact on-disk work format.
pub(crate) fn work_entries(entries: Vec<FileEntry>) -> Vec<WorkEntry> {
    entries
        .into_iter()
        .map(|entry| WorkEntry {
            path: entry.path.to_string_lossy().into_owned(),
            kind: entry.kind,
            size: entry.size,
            mtime: entry.mtime_ns,
            inode: entry.inode,
        })
        .collect()
}
