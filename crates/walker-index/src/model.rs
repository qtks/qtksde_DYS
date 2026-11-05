use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const INDEX_SCHEMA_VERSION: &str = "panopticon.m1.index.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeStatus {
    Online,
    Offline,
}

impl VolumeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            VolumeStatus::Online => "online",
            VolumeStatus::Offline => "offline",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "online" => Some(VolumeStatus::Online),
            "offline" => Some(VolumeStatus::Offline),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Volume {
    pub id: String,
    pub name: String,
    pub mount_path: PathBuf,
    pub device: Option<u64>,
    pub status: VolumeStatus,
    pub last_seen_at: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    Collecting,
    Writing,
    Done,
}

impl SessionPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionPhase::Collecting => "collecting",
            SessionPhase::Writing => "writing",
            SessionPhase::Done => "done",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "collecting" => Some(SessionPhase::Collecting),
            "writing" => Some(SessionPhase::Writing),
            "done" => Some(SessionPhase::Done),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Running,
    Completed,
    Aborted,
}

impl SessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionStatus::Running => "running",
            SessionStatus::Completed => "completed",
            SessionStatus::Aborted => "aborted",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScanSession {
    pub id: String,
    pub volume_id: String,
    pub root_path: String,
    pub phase: SessionPhase,
    pub status: SessionStatus,
    pub work_file: Option<PathBuf>,
    pub dirs_file: Option<PathBuf>,
    pub work_file_offset: u64,
    pub total_entries: u64,
    pub completed_batches: u64,
    pub batch_size: u64,
    pub cursor_path: Option<String>,
    pub started_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
}

/// Tunables for one indexing run. Production code should use the default
/// `batch_size` of 5000; tests shrink batches and inject faults.
#[derive(Debug, Clone)]
pub struct IndexOptions {
    pub db_path: PathBuf,
    pub root: PathBuf,
    pub extra_excludes: Vec<String>,
    pub use_default_excludes: bool,
    pub follow_links: bool,
    pub batch_size: usize,
    /// Resume an interrupted session for this volume instead of starting over.
    pub resume: bool,
    /// Fault injection: stop (leaving the checkpoint) after this many batches.
    pub abort_after_batches: Option<u64>,
    /// Fault injection: fail inside the very first batch after N applied rows.
    pub fail_during_batch_after: Option<u64>,
    pub threads: Option<usize>,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            db_path: PathBuf::new(),
            root: PathBuf::new(),
            extra_excludes: Vec::new(),
            use_default_excludes: true,
            follow_links: false,
            batch_size: 5000,
            resume: true,
            abort_after_batches: None,
            fail_during_batch_after: None,
            threads: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexStats {
    pub entries_total: u64,
    pub inserted: u64,
    pub updated: u64,
    pub unchanged: u64,
    pub marked_missing: u64,
    pub cascaded_missing: u64,
    /// Data-row writes (insert + update + missing), excluding checkpoint rows.
    pub rows_written: u64,
    pub batches_committed: u64,
    pub restored_offline_rows: u64,
    pub resumed: bool,
    pub resumed_from_batch: u64,
    pub traversal_ms: u64,
    pub serialize_ms: u64,
    /// Batch insert/update time, including the final WAL checkpoint.
    pub write_ms: u64,
    /// Time spent rebuilding secondary indexes on a fresh-volume bulk load.
    /// Already contained in `write_ms`.
    pub index_build_ms: u64,
    pub restore_ms: u64,
    pub total_ms: u64,
}

#[derive(Debug, Clone)]
pub struct IndexOutcome {
    pub volume: Volume,
    pub session_id: String,
    pub report: Option<walker_core::ScanReport>,
    pub stats: IndexStats,
}
