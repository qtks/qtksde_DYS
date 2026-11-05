use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use walkdir::{DirEntry, WalkDir};

use crate::case;
use crate::exclude::{ExcludeMatcher, DEFAULT_EXCLUDES};
use crate::fsmeta::{device_id, inode, nlink};
use crate::model::{
    CaseProbe, Distributions, EntryKind, ExclusionReport, ExtensionBucket, FileEntry,
    HardlinkSummary, OptionsReport, RootInfo, ScanOutcome, ScanReport, SizeBucket, SkipKind,
    SkippedItem, SkippedReport, Summary, Timings, ToolInfo, SCHEMA_VERSION,
};
use crate::name::{extension_key, normalize_name};

const MAX_LOCAL_ERRORS: usize = 256;
const BUCKET_COUNT: usize = 6;

const SIZE_BUCKETS: [(&str, u64, Option<u64>); BUCKET_COUNT] = [
    ("0 B", 0, Some(1)),
    ("1 B - 1 KiB", 1, Some(1024)),
    ("1 KiB - 1 MiB", 1024, Some(1024 * 1024)),
    ("1 MiB - 100 MiB", 1024 * 1024, Some(100 * 1024 * 1024)),
    (
        "100 MiB - 1 GiB",
        100 * 1024 * 1024,
        Some(1024 * 1024 * 1024),
    ),
    (">= 1 GiB", 1024 * 1024 * 1024, None),
];

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub roots: Vec<PathBuf>,
    /// Extra exclusion patterns appended to the defaults.
    pub extra_excludes: Vec<String>,
    pub use_default_excludes: bool,
    /// Follow symbolic links to directories, with visited-inode cycle detection.
    pub follow_links: bool,
    pub skipped_item_limit: usize,
    pub threads: Option<usize>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            extra_excludes: Vec::new(),
            use_default_excludes: true,
            follow_links: false,
            skipped_item_limit: MAX_LOCAL_ERRORS,
            threads: None,
        }
    }
}

/// Read-only scan. All IO errors are converted into report entries; the
/// function does not panic on filesystem errors.
pub fn scan(options: &ScanOptions) -> ScanReport {
    scan_impl(options).report
}

/// Same traversal as [`scan`], additionally returning one lean record per
/// entry so the indexer can persist filesystem metadata. `scan` stays
/// allocation-light by discarding these records.
pub fn scan_with_entries(options: &ScanOptions) -> ScanOutcome {
    scan_impl(options)
}

fn scan_impl(options: &ScanOptions) -> ScanOutcome {
    let total_start = Instant::now();

    if let Some(threads) = options.threads.filter(|value| *value > 0) {
        if let Ok(pool) = rayon::ThreadPoolBuilder::new().num_threads(threads).build() {
            return pool.install(|| scan_inner(options, total_start));
        }
    }
    scan_inner(options, total_start)
}

fn scan_inner(options: &ScanOptions, total_start: Instant) -> ScanOutcome {
    let setup_start = Instant::now();

    let mut patterns: Vec<String> = Vec::new();
    if options.use_default_excludes {
        patterns.extend(
            DEFAULT_EXCLUDES
                .iter()
                .map(|pattern| (*pattern).to_string()),
        );
    }
    patterns.extend(options.extra_excludes.iter().cloned());

    let context = Context {
        excludes: ExcludeMatcher::new(&patterns),
        follow_links: options.follow_links,
        visited_directories: Mutex::new(HashSet::new()),
    };

    let mut roots = Vec::with_capacity(options.roots.len());
    let mut root_stats = LocalStats::default();
    let mut root_directories: Vec<(PathBuf, bool)> = Vec::new();

    for root in &options.roots {
        let probe = case::probe(root);
        match fs::metadata(root) {
            Ok(metadata) => {
                roots.push(RootInfo {
                    path: path_string(root),
                    accessible: true,
                    case_sensitivity: probe,
                });
                if metadata.is_dir() {
                    let key = inode_key(&metadata);
                    if context.claim_directory(key) {
                        root_stats.directories += 1;
                        let scan_path = match fs::canonicalize(root) {
                            Ok(path) => path,
                            Err(error) => {
                                root_stats.record_io_error(root, &error, "root_canonicalize");
                                root.clone()
                            }
                        };
                        root_directories.push((scan_path, true));
                    }
                } else if metadata.is_file() {
                    root_stats.record_file(root, &metadata);
                } else {
                    root_stats.other_entries += 1;
                }
            }
            Err(error) => {
                roots.push(RootInfo {
                    path: path_string(root),
                    accessible: false,
                    case_sensitivity: CaseProbe::unknown("unavailable"),
                });
                root_stats.record_io_error(root, &error, "root_metadata");
            }
        }
    }

    let setup_ms = elapsed_ms(setup_start);

    let walk_start = Instant::now();
    let walked = scan_children(&context, &root_directories);
    root_stats.merge(walked);
    let walk_ms = elapsed_ms(walk_start);

    let aggregate_start = Instant::now();
    let entries = std::mem::take(&mut root_stats.entries);
    let listed_directories = std::mem::take(&mut root_stats.listed_directories);
    let mut report = build_report(root_stats, roots, options, patterns, setup_ms, walk_ms);
    report.timings.aggregate_ms = elapsed_ms(aggregate_start);
    report.timings.total_wall_ms = elapsed_ms(total_start);
    report.timings.files_per_sec = if report.timings.walk_ms == 0 {
        0.0
    } else {
        report.summary.files as f64 / (report.timings.walk_ms as f64 / 1000.0)
    };
    ScanOutcome {
        report,
        entries,
        listed_directories,
    }
}

fn build_report(
    stats: LocalStats,
    roots: Vec<RootInfo>,
    options: &ScanOptions,
    patterns: Vec<String>,
    setup_ms: u64,
    walk_ms: u64,
) -> ScanReport {
    let unique_hardlink_inodes = stats.hardlink_inodes.len() as u64;
    let unique_hardlink_bytes: u64 = stats.hardlink_inodes.values().copied().sum();
    let deduplicated_bytes = stats
        .hardlinked_bytes_logical
        .saturating_sub(unique_hardlink_bytes);
    let total_bytes = stats.logical_bytes.saturating_sub(deduplicated_bytes);
    let unique_file_inodes = stats
        .files
        .saturating_sub(stats.hardlinked_files)
        .saturating_add(unique_hardlink_inodes);

    let mut by_extension: Vec<ExtensionBucket> = stats
        .ext_files
        .into_iter()
        .map(|(extension, bucket)| ExtensionBucket {
            extension,
            files: bucket.files,
            bytes: bucket.bytes,
        })
        .collect();
    by_extension.sort_by(|left, right| {
        right
            .files
            .cmp(&left.files)
            .then_with(|| left.extension.cmp(&right.extension))
    });

    let by_size = SIZE_BUCKETS
        .iter()
        .zip(stats.buckets.iter())
        .map(
            |((label, min_bytes, max_bytes_exclusive), bucket)| SizeBucket {
                label: (*label).to_string(),
                min_bytes: *min_bytes,
                max_bytes_exclusive: *max_bytes_exclusive,
                files: bucket.files,
                bytes: bucket.bytes,
            },
        )
        .collect();

    let mut items = stats.errors;
    items.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.kind.cmp(&right.kind))
            .then_with(|| left.operation.cmp(&right.operation))
    });
    let limit = options.skipped_item_limit;
    if items.len() > limit {
        items.truncate(limit);
    }

    ScanReport {
        schema_version: SCHEMA_VERSION.to_string(),
        tool: ToolInfo {
            name: env!("CARGO_PKG_NAME").to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
        roots,
        summary: Summary {
            files: stats.files,
            directories: stats.directories,
            symlinks: stats.symlinks,
            other_entries: stats.other_entries,
            unique_file_inodes,
            logical_bytes: stats.logical_bytes,
            total_bytes,
            earliest_mtime_unix: stats.earliest_mtime,
            latest_mtime_unix: stats.latest_mtime,
            hardlinked_files: stats.hardlinked_files,
        },
        distributions: Distributions {
            by_extension,
            by_size,
        },
        hardlinks: HardlinkSummary {
            hardlinked_files: stats.hardlinked_files,
            unique_hardlink_inodes,
            deduplicated_bytes,
        },
        exclusions: ExclusionReport {
            patterns,
            default_patterns: DEFAULT_EXCLUDES
                .iter()
                .map(|pattern| (*pattern).to_string())
                .collect(),
            excluded_directories: stats.excluded_directories,
        },
        skipped: SkippedReport {
            total: stats.errors_total,
            by_kind: stats.errors_by_kind,
            limit: limit as u64,
            truncated: stats.errors_total > items.len() as u64,
            items,
        },
        options: OptionsReport {
            follow_links: options.follow_links,
            use_default_excludes: options.use_default_excludes,
            extra_excludes: options.extra_excludes.clone(),
            threads: options.threads,
        },
        timings: Timings {
            setup_ms,
            walk_ms,
            aggregate_ms: 0,
            total_wall_ms: 0,
            files_per_sec: 0.0,
        },
    }
}

struct Context {
    excludes: ExcludeMatcher,
    follow_links: bool,
    visited_directories: Mutex<HashSet<InodeKey>>,
}

impl Context {
    fn claim_directory(&self, key: InodeKey) -> bool {
        self.lock_visited().insert(key)
    }

    fn lock_visited(&self) -> MutexGuard<'_, HashSet<InodeKey>> {
        match self.visited_directories.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct InodeKey {
    device: u64,
    inode: u64,
}

fn inode_key(metadata: &Metadata) -> InodeKey {
    InodeKey {
        device: device_id(metadata).unwrap_or(0),
        inode: inode(metadata).unwrap_or(0),
    }
}

/// Traverse a list of directories, splitting the work with `rayon::join`.
fn scan_children(context: &Context, directories: &[(PathBuf, bool)]) -> LocalStats {
    match directories.len() {
        0 => LocalStats::default(),
        1 => scan_directory(context, &directories[0].0, directories[0].1),
        length => {
            let (left, right) = directories.split_at(length / 2);
            let (mut left_stats, right_stats) = rayon::join(
                || scan_children(context, left),
                || scan_children(context, right),
            );
            left_stats.merge(right_stats);
            left_stats
        }
    }
}

fn scan_directory(context: &Context, directory: &Path, preclaimed: bool) -> LocalStats {
    let mut stats = LocalStats::default();

    let metadata = match fs::metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) => {
            stats.record_io_error(directory, &error, "dir_metadata");
            return stats;
        }
    };
    if !metadata.is_dir() || (!preclaimed && !context.claim_directory(inode_key(&metadata))) {
        return stats;
    }

    // Emitted only for directories actually entered, so "listed" is truthful.
    stats.entries.push(entry_from_metadata(
        directory,
        EntryKind::Directory,
        &metadata,
    ));

    let mut children: Vec<(PathBuf, bool)> = Vec::new();
    let mut listing_complete = true;

    // One directory per walk; subdirectories are handed to rayon recursively.
    let walker = WalkDir::new(directory)
        .min_depth(1)
        .max_depth(1)
        .follow_links(false)
        .into_iter();
    for item in walker {
        match item {
            Ok(entry) => {
                let file_type = entry.file_type();
                if file_type.is_dir() {
                    stats.directories += 1;
                    let path = entry.into_path();
                    if context.excludes.is_excluded(&path) {
                        stats.excluded_directories += 1;
                    } else {
                        children.push((path, false));
                    }
                } else if file_type.is_symlink() {
                    stats.symlinks += 1;
                    match entry.metadata() {
                        Ok(target_metadata) => stats.entries.push(entry_from_metadata(
                            entry.path(),
                            EntryKind::Symlink,
                            &target_metadata,
                        )),
                        Err(error) => match error.io_error() {
                            Some(io_error) => {
                                stats.record_io_error(entry.path(), io_error, "symlink_metadata")
                            }
                            None => stats.record_walkdir_error(entry.path(), &error),
                        },
                    }
                    if context.follow_links {
                        if let Some(child) = resolve_symlink_directory(context, &entry, &mut stats)
                        {
                            children.push(child);
                        }
                    }
                } else if file_type.is_file() {
                    match entry.metadata() {
                        Ok(metadata) => stats.record_file(entry.path(), &metadata),
                        Err(error) => match error.io_error() {
                            Some(io_error) => {
                                stats.record_io_error(entry.path(), io_error, "entry_metadata")
                            }
                            None => stats.record_walkdir_error(entry.path(), &error),
                        },
                    }
                } else {
                    stats.other_entries += 1;
                }
            }
            Err(error) => {
                listing_complete = false;
                let path = error
                    .path()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| directory.to_path_buf());
                stats.record_walkdir_error(&path, &error);
            }
        }
    }

    if listing_complete {
        stats.listed_directories.push(directory.to_path_buf());
    }

    let child_stats = scan_children(context, &children);
    stats.merge(child_stats);
    stats
}

fn resolve_symlink_directory(
    context: &Context,
    entry: &DirEntry,
    stats: &mut LocalStats,
) -> Option<(PathBuf, bool)> {
    let path = entry.path();
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            stats.record_io_error(path, &error, "symlink_target_metadata");
            return None;
        }
    };
    if !metadata.is_dir() {
        return None;
    }
    let key = inode_key(&metadata);
    if !context.claim_directory(key) {
        stats.record_skip(
            path,
            SkipKind::SymlinkCycle,
            "follow_symlink",
            format!(
                "target inode {}:{} already visited (cycle or duplicate)",
                key.device, key.inode
            ),
        );
        return None;
    }
    match fs::canonicalize(path) {
        Ok(target) => Some((target, true)),
        Err(error) => {
            stats.record_io_error(path, &error, "symlink_canonicalize");
            None
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct BucketAcc {
    files: u64,
    bytes: u64,
}

impl BucketAcc {
    fn add_file(&mut self, size: u64) {
        self.files += 1;
        self.bytes = self.bytes.saturating_add(size);
    }

    fn merge(&mut self, other: BucketAcc) {
        self.files += other.files;
        self.bytes = self.bytes.saturating_add(other.bytes);
    }
}

#[derive(Debug, Default)]
struct LocalStats {
    files: u64,
    directories: u64,
    symlinks: u64,
    other_entries: u64,
    logical_bytes: u64,
    hardlinked_files: u64,
    hardlinked_bytes_logical: u64,
    hardlink_inodes: HashMap<InodeKey, u64>,
    ext_files: BTreeMap<String, BucketAcc>,
    buckets: [BucketAcc; BUCKET_COUNT],
    errors: Vec<SkippedItem>,
    errors_total: u64,
    errors_by_kind: BTreeMap<String, u64>,
    excluded_directories: u64,
    earliest_mtime: Option<i64>,
    latest_mtime: Option<i64>,
    entries: Vec<FileEntry>,
    listed_directories: Vec<PathBuf>,
}

impl LocalStats {
    fn record_file(&mut self, path: &Path, metadata: &Metadata) {
        let name = path
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name_nfc = normalize_name(&name);
        let extension = extension_key(&name_nfc);
        let size = metadata.len();

        self.files += 1;
        self.logical_bytes = self.logical_bytes.saturating_add(size);
        self.ext_files.entry(extension).or_default().add_file(size);
        self.buckets[size_bucket(size)].add_file(size);
        self.entries
            .push(entry_from_metadata(path, EntryKind::File, metadata));

        if nlink(metadata) > 1 {
            self.hardlinked_files += 1;
            self.hardlinked_bytes_logical = self.hardlinked_bytes_logical.saturating_add(size);
            self.hardlink_inodes
                .entry(inode_key(metadata))
                .or_insert(size);
        }

        if let Some(seconds) = unix_mtime_seconds(metadata) {
            self.earliest_mtime = Some(
                self.earliest_mtime
                    .map_or(seconds, |current| current.min(seconds)),
            );
            self.latest_mtime = Some(
                self.latest_mtime
                    .map_or(seconds, |current| current.max(seconds)),
            );
        }
    }

    fn record_io_error(&mut self, path: &Path, error: &io::Error, operation: &str) {
        let kind = if error.kind() == io::ErrorKind::PermissionDenied {
            SkipKind::PermissionDenied
        } else {
            SkipKind::IoError
        };
        self.record_skip(path, kind, operation, error.to_string());
    }

    fn record_walkdir_error(&mut self, path: &Path, error: &walkdir::Error) {
        let kind = match error.io_error().map(io::Error::kind) {
            Some(io::ErrorKind::PermissionDenied) => SkipKind::PermissionDenied,
            _ => SkipKind::IoError,
        };
        self.record_skip(path, kind, "walk", error.to_string());
    }

    fn record_skip(&mut self, path: &Path, kind: SkipKind, operation: &str, message: String) {
        self.errors_total += 1;
        *self
            .errors_by_kind
            .entry(kind.as_str().to_string())
            .or_insert(0) += 1;
        if self.errors.len() < MAX_LOCAL_ERRORS {
            self.errors.push(SkippedItem {
                path: path_string(path),
                kind,
                operation: operation.to_string(),
                message,
            });
        }
    }

    fn merge(&mut self, other: LocalStats) {
        self.files += other.files;
        self.directories += other.directories;
        self.symlinks += other.symlinks;
        self.other_entries += other.other_entries;
        self.logical_bytes = self.logical_bytes.saturating_add(other.logical_bytes);
        self.hardlinked_files += other.hardlinked_files;
        self.hardlinked_bytes_logical = self
            .hardlinked_bytes_logical
            .saturating_add(other.hardlinked_bytes_logical);
        self.excluded_directories += other.excluded_directories;

        for (key, size) in other.hardlink_inodes {
            self.hardlink_inodes.entry(key).or_insert(size);
        }
        for (extension, bucket) in other.ext_files {
            self.ext_files.entry(extension).or_default().merge(bucket);
        }
        for (index, bucket) in other.buckets.iter().enumerate() {
            self.buckets[index].merge(*bucket);
        }

        self.errors_total += other.errors_total;
        for (kind, count) in other.errors_by_kind {
            *self.errors_by_kind.entry(kind).or_insert(0) += count;
        }
        for item in other.errors {
            if self.errors.len() >= MAX_LOCAL_ERRORS {
                break;
            }
            self.errors.push(item);
        }

        self.earliest_mtime = match (self.earliest_mtime, other.earliest_mtime) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (left, right) => left.or(right),
        };
        self.latest_mtime = match (self.latest_mtime, other.latest_mtime) {
            (Some(left), Some(right)) => Some(left.max(right)),
            (left, right) => left.or(right),
        };
        self.entries.extend(other.entries);
        self.listed_directories.extend(other.listed_directories);
    }
}

fn size_bucket(size: u64) -> usize {
    match size {
        0 => 0,
        value if value < 1024 => 1,
        value if value < 1024 * 1024 => 2,
        value if value < 100 * 1024 * 1024 => 3,
        value if value < 1024 * 1024 * 1024 => 4,
        _ => 5,
    }
}

fn entry_from_metadata(path: &Path, kind: EntryKind, metadata: &Metadata) -> FileEntry {
    FileEntry {
        path: path.to_path_buf(),
        kind,
        size: metadata.len(),
        mtime_ns: unix_mtime_nanos(metadata),
        inode: inode(metadata).unwrap_or(0),
        nlink: nlink(metadata),
    }
}

fn unix_mtime_nanos(metadata: &Metadata) -> i64 {
    let modified: SystemTime = match metadata.modified() {
        Ok(value) => value,
        Err(_) => return 0,
    };
    match modified.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos() as i64,
        Err(error) => -(error.duration().as_nanos() as i64),
    }
}

fn unix_mtime_seconds(metadata: &Metadata) -> Option<i64> {
    let modified: SystemTime = metadata.modified().ok()?;
    match modified.duration_since(UNIX_EPOCH) {
        Ok(duration) => Some(duration.as_secs() as i64),
        Err(error) => Some(-(error.duration().as_secs() as i64)),
    }
}

fn elapsed_ms(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
