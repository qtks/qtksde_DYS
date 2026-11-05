use std::fs;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use walker_core::{
    normalize_name, scan, scan_with_entries, CaseSensitivity, EntryKind, ScanOptions, SkipKind,
};

fn options(root: &Path) -> ScanOptions {
    ScanOptions {
        roots: vec![root.to_path_buf()],
        ..ScanOptions::default()
    }
}

struct PermissionGuard {
    path: PathBuf,
    mode: u32,
}

impl Drop for PermissionGuard {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(self.mode));
    }
}

fn chmod(path: &Path, mode: u32) -> PermissionGuard {
    let original = fs::metadata(path).expect("metadata").mode() & 0o777;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
    PermissionGuard {
        path: path.to_path_buf(),
        mode: original,
    }
}

#[test]
fn detects_symlink_cycle_without_hanging() {
    let root = TempDir::new().expect("tempdir");
    let a = root.path().join("a");
    fs::create_dir(&a).expect("mkdir a");
    fs::write(a.join("data.txt"), b"payload").expect("write file");
    symlink(&a, a.join("loop")).expect("symlink loop");

    let report = scan(&ScanOptions {
        follow_links: true,
        ..options(root.path())
    });

    assert_eq!(report.summary.files, 1);
    assert_eq!(report.summary.symlinks, 1);
    assert!(report.summary.directories <= 3);
    assert_eq!(report.skipped.total, 1);
    assert_eq!(report.skipped.by_kind.get("symlink_cycle"), Some(&1));
    let item = report
        .skipped
        .items
        .iter()
        .find(|item| item.kind == SkipKind::SymlinkCycle)
        .expect("symlink cycle item");
    assert!(item.path.ends_with("loop"), "path was {}", item.path);
}

#[test]
fn does_not_follow_symlinks_by_default() {
    let root = TempDir::new().expect("tempdir");
    let a = root.path().join("a");
    fs::create_dir(&a).expect("mkdir a");
    symlink(&a, a.join("loop")).expect("symlink loop");

    let report = scan(&options(root.path()));
    assert_eq!(report.summary.symlinks, 1);
    assert_eq!(report.skipped.total, 0);
}

#[test]
fn records_permission_denied_directory() {
    let root = TempDir::new().expect("tempdir");
    let blocked = root.path().join("blocked");
    fs::create_dir(&blocked).expect("mkdir blocked");
    fs::write(blocked.join("secret.txt"), b"secret").expect("write secret");
    let _guard = chmod(&blocked, 0o000);

    if fs::read_dir(&blocked).is_ok() {
        // Running with elevated privileges makes this test unobservable.
        return;
    }

    let report = scan(&options(root.path()));
    assert_eq!(report.skipped.total, 1);
    assert_eq!(
        report.skipped.by_kind.get("permission_denied"),
        Some(&1),
        "report: {:?}",
        report.skipped
    );
    assert_eq!(report.skipped.items[0].kind, SkipKind::PermissionDenied);
}

#[test]
fn handles_chinese_and_emoji_file_names() {
    let root = TempDir::new().expect("tempdir");
    fs::write(root.path().join("中文文档😀.txt"), b"hello").expect("chinese name");
    fs::write(root.path().join("cafe\u{301}.md"), b"nfc").expect("decomposed name");

    let report = scan(&options(root.path()));
    assert_eq!(report.summary.files, 2);
    let extensions: Vec<&str> = report
        .distributions
        .by_extension
        .iter()
        .map(|bucket| bucket.extension.as_str())
        .collect();
    assert!(extensions.contains(&"txt"));
    assert!(extensions.contains(&"md"));
    assert_eq!(normalize_name("cafe\u{301}.md"), "café.md");
    assert_eq!(normalize_name("cafe\u{301}.md"), normalize_name("café.md"));
}

#[test]
fn handles_very_long_paths() {
    let root = TempDir::new().expect("tempdir");
    let mut deep = root.path().to_path_buf();
    for index in 0..25 {
        deep.push(format!("segment_{index:02}_{}", "x".repeat(40)));
        fs::create_dir(&deep).expect("mkdir deep");
    }
    assert!(
        deep.as_os_str().len() > 1024,
        "path length {}",
        deep.as_os_str().len()
    );
    fs::write(deep.join("tail.txt"), b"long").expect("write tail");

    let report = scan(&options(root.path()));
    assert_eq!(report.summary.files, 1);
    assert_eq!(report.summary.directories, 26);
    assert_eq!(report.skipped.total, 0);
}

#[test]
fn detects_case_sensitivity_and_counts_case_variants() {
    let root = TempDir::new().expect("tempdir");
    let upper = root.path().join("CaseName.txt");
    let lower = root.path().join("casename.txt");
    fs::write(&upper, b"upper").expect("write upper");
    fs::write(&lower, b"lower").expect("write lower");

    let upper_ino = fs::metadata(&upper).expect("upper metadata").ino();
    let lower_ino = fs::metadata(&lower).expect("lower metadata").ino();
    let distinct = upper_ino != lower_ino;

    let report = scan(&options(root.path()));
    assert_eq!(report.summary.files, if distinct { 2 } else { 1 });
    if distinct {
        assert_eq!(
            report.roots[0].case_sensitivity.sensitivity,
            CaseSensitivity::Sensitive
        );
    } else {
        assert_eq!(
            report.roots[0].case_sensitivity.sensitivity,
            CaseSensitivity::Insensitive
        );
    }
}

#[test]
fn counts_hardlinks_once_for_bytes() {
    let root = TempDir::new().expect("tempdir");
    let original = root.path().join("original.bin");
    fs::write(&original, vec![7u8; 4096]).expect("write original");
    fs::hard_link(&original, root.path().join("alias.bin")).expect("hard link");

    let report = scan(&options(root.path()));
    assert_eq!(report.summary.files, 2);
    assert_eq!(report.summary.logical_bytes, 8192);
    assert_eq!(report.summary.total_bytes, 4096);
    assert_eq!(report.summary.unique_file_inodes, 1);
    assert_eq!(report.hardlinks.hardlinked_files, 2);
    assert_eq!(report.hardlinks.unique_hardlink_inodes, 1);
    assert_eq!(report.hardlinks.deduplicated_bytes, 4096);
    assert_eq!(report.distributions.by_extension[0].files, 2);
    assert_eq!(report.distributions.by_extension[0].bytes, 8192);
}

#[test]
fn applies_default_and_extra_exclusions() {
    let root = TempDir::new().expect("tempdir");
    fs::create_dir_all(root.path().join("node_modules/pkg")).expect("node_modules");
    fs::write(root.path().join("node_modules/pkg/index.js"), b"js").expect("js file");
    fs::create_dir_all(root.path().join(".git/objects/ab")).expect("git objects");
    fs::write(root.path().join(".git/objects/ab/object"), b"obj").expect("object");
    fs::write(root.path().join(".git/config"), b"git").expect("config");
    fs::create_dir_all(root.path().join("src")).expect("src");
    fs::write(root.path().join("src/main.rs"), b"rs").expect("main");

    let report = scan(&options(root.path()));
    assert_eq!(report.exclusions.excluded_directories, 2);
    assert_eq!(report.summary.files, 2);
    assert_eq!(report.skipped.total, 0);

    let extra = scan(&ScanOptions {
        extra_excludes: vec!["src".to_string()],
        ..options(root.path())
    });
    assert_eq!(extra.exclusions.excluded_directories, 3);
    assert_eq!(extra.summary.files, 1);

    let no_defaults = scan(&ScanOptions {
        use_default_excludes: false,
        ..options(root.path())
    });
    assert_eq!(no_defaults.exclusions.excluded_directories, 0);
    assert_eq!(no_defaults.summary.files, 4);
}

#[test]
fn missing_root_is_reported_not_panicked() {
    let root = TempDir::new().expect("tempdir");
    let missing = root.path().join("does-not-exist");

    let report = scan(&ScanOptions {
        roots: vec![missing.clone()],
        ..ScanOptions::default()
    });
    assert_eq!(report.roots.len(), 1);
    assert!(!report.roots[0].accessible);
    assert_eq!(report.summary.files, 0);
    assert_eq!(report.skipped.total, 1);
    assert_eq!(report.skipped.by_kind.get("io_error"), Some(&1));
}

#[test]
fn scans_a_file_root() {
    let root = TempDir::new().expect("tempdir");
    let file = root.path().join("single.rs");
    fs::write(&file, b"fn main() {}").expect("write");

    let report = scan(&ScanOptions {
        roots: vec![file],
        ..ScanOptions::default()
    });
    assert_eq!(report.summary.files, 1);
    assert_eq!(report.summary.directories, 0);
    assert_eq!(report.summary.total_bytes, 12);
}

#[test]
fn report_serializes_to_stable_json_shape() {
    let root = TempDir::new().expect("tempdir");
    fs::write(root.path().join("a.txt"), b"a").expect("write");
    let report = scan(&options(root.path()));

    let json = serde_json::to_string_pretty(&report).expect("serialize");
    let parsed: walker_core::ScanReport = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(parsed, report);
    for field in [
        "\"schema_version\"",
        "\"summary\"",
        "\"distributions\"",
        "\"hardlinks\"",
        "\"exclusions\"",
        "\"skipped\"",
        "\"timings\"",
        "\"total_wall_ms\"",
        "\"by_extension\"",
        "\"by_size\"",
    ] {
        assert!(json.contains(field), "missing {field} in {json}");
    }
}

#[test]
fn scan_with_entries_emits_every_entry_and_listed_directories() {
    let root = TempDir::new().expect("tempdir");
    fs::create_dir_all(root.path().join("a/b")).expect("mkdir");
    fs::write(root.path().join("a/b/file.txt"), b"data").expect("write");
    fs::write(root.path().join("top.txt"), b"top").expect("write");

    let outcome = scan_with_entries(&options(root.path()));
    assert_eq!(outcome.report.summary.files, 2);
    assert_eq!(outcome.report.summary.directories, 3);
    assert_eq!(outcome.entries.len(), 5);
    assert_eq!(
        outcome
            .entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::Directory)
            .count(),
        3
    );

    let file = outcome
        .entries
        .iter()
        .find(|entry| entry.path.ends_with("file.txt"))
        .expect("file entry");
    assert_eq!(file.size, 4);
    assert_eq!(file.file_type(), "file");
    assert!(file.mtime_ns > 0);
    assert!(file.inode > 0);

    assert_eq!(outcome.listed_directories.len(), 3);
    assert_eq!(scan(&options(root.path())).summary.files, 2);
}
