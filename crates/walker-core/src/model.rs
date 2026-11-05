use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SCHEMA_VERSION: &str = "panopticon.m0.scan.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseSensitivity {
    Sensitive,
    Insensitive,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseProbe {
    pub sensitivity: CaseSensitivity,
    pub method: String,
    pub evidence: Option<String>,
}

impl CaseProbe {
    pub(crate) fn unknown(method: &str) -> Self {
        Self {
            sensitivity: CaseSensitivity::Unknown,
            method: method.to_string(),
            evidence: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipKind {
    PermissionDenied,
    IoError,
    SymlinkCycle,
}

impl SkipKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipKind::PermissionDenied => "permission_denied",
            SkipKind::IoError => "io_error",
            SkipKind::SymlinkCycle => "symlink_cycle",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedItem {
    pub path: String,
    pub kind: SkipKind,
    pub operation: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedReport {
    pub total: u64,
    pub by_kind: BTreeMap<String, u64>,
    pub limit: u64,
    pub truncated: bool,
    pub items: Vec<SkippedItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootInfo {
    pub path: String,
    pub accessible: bool,
    pub case_sensitivity: CaseProbe,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub files: u64,
    pub directories: u64,
    pub symlinks: u64,
    pub other_entries: u64,
    pub unique_file_inodes: u64,
    pub logical_bytes: u64,
    pub total_bytes: u64,
    pub earliest_mtime_unix: Option<i64>,
    pub latest_mtime_unix: Option<i64>,
    pub hardlinked_files: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionBucket {
    pub extension: String,
    pub files: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizeBucket {
    pub label: String,
    pub min_bytes: u64,
    pub max_bytes_exclusive: Option<u64>,
    pub files: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Distributions {
    pub by_extension: Vec<ExtensionBucket>,
    pub by_size: Vec<SizeBucket>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardlinkSummary {
    pub hardlinked_files: u64,
    pub unique_hardlink_inodes: u64,
    pub deduplicated_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExclusionReport {
    pub patterns: Vec<String>,
    pub default_patterns: Vec<String>,
    pub excluded_directories: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionsReport {
    pub follow_links: bool,
    pub use_default_excludes: bool,
    pub extra_excludes: Vec<String>,
    pub threads: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Timings {
    pub setup_ms: u64,
    pub walk_ms: u64,
    pub aggregate_ms: u64,
    pub total_wall_ms: u64,
    pub files_per_sec: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanReport {
    pub schema_version: String,
    pub tool: ToolInfo,
    pub roots: Vec<RootInfo>,
    pub summary: Summary,
    pub distributions: Distributions,
    pub hardlinks: HardlinkSummary,
    pub exclusions: ExclusionReport,
    pub skipped: SkippedReport,
    pub options: OptionsReport,
    pub timings: Timings,
}
