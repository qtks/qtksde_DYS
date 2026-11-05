use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use rusqlite::params;
use tempfile::TempDir;

use walker_index::{index_root, poll_volume, IndexDb, IndexError, IndexOptions, VolumeEvent};

fn options(db: &Path, root: &Path) -> IndexOptions {
    IndexOptions {
        db_path: db.to_path_buf(),
        root: root.to_path_buf(),
        ..IndexOptions::default()
    }
}

fn create_files(directory: &Path, count: usize, prefix: &str) -> Vec<PathBuf> {
    fs::create_dir_all(directory).unwrap();
    (0..count)
        .map(|index| {
            let path = directory.join(format!("{prefix}{index:05}.txt"));
            fs::write(&path, b"x").unwrap();
            path
        })
        .collect()
}

fn count(db: &IndexDb, sql: &str, parameters: impl rusqlite::Params) -> i64 {
    db.connection()
        .query_row(sql, parameters, |row| row.get(0))
        .unwrap()
}

fn count_in(db_path: &Path, sql: &str) -> i64 {
    let db = IndexDb::open(db_path).unwrap();
    count(&db, sql, [])
}

fn canonical(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

#[test]
fn creates_schema_wal_and_required_indexes() {
    let directory = TempDir::new().unwrap();
    let db = IndexDb::open(&directory.path().join("index.sqlite")).unwrap();

    assert_eq!(db.journal_mode().unwrap().to_lowercase(), "wal");

    let indexes: Vec<String> = db
        .connection()
        .prepare("SELECT name FROM sqlite_master WHERE type='index' AND tbl_name='files'")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|row| row.unwrap())
        .collect();

    for required in [
        "idx_files_path",
        "idx_files_parent",
        "idx_files_hash",
        "idx_files_norm_name",
    ] {
        assert!(
            indexes.contains(&required.to_string()),
            "missing {required}"
        );
    }

    let hash_sql: String = db
        .connection()
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name='idx_files_hash'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        hash_sql
            .to_uppercase()
            .contains("WHERE CONTENT_HASH IS NOT NULL"),
        "idx_files_hash must be a partial index: {hash_sql}"
    );

    let tables: Vec<String> = db
        .connection()
        .prepare("SELECT name FROM sqlite_master WHERE type='table'")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|row| row.unwrap())
        .collect();
    for table in ["volumes", "files", "scan_sessions"] {
        assert!(tables.contains(&table.to_string()), "missing table {table}");
    }
}

#[test]
fn bulk_load_rebuilds_the_secondary_indexes() {
    let directory = TempDir::new().unwrap();
    let root = directory.path().join("volume");
    create_files(&root, 10, "f");
    let db_path = directory.path().join("index.sqlite");

    // A fresh volume skips these indexes during the insert phase.
    let outcome = index_root(&options(&db_path, &root)).unwrap();
    assert!(outcome.stats.index_build_ms <= outcome.stats.write_ms);

    let db = IndexDb::open(&db_path).unwrap();
    let indexes: Vec<String> = db
        .connection()
        .prepare("SELECT name FROM sqlite_master WHERE type='index' AND tbl_name='files'")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|row| row.unwrap())
        .collect();
    for required in [
        "idx_files_path",
        "idx_files_parent",
        "idx_files_hash",
        "idx_files_norm_name",
    ] {
        assert!(
            indexes.contains(&required.to_string()),
            "missing {required} after bulk load"
        );
    }
}

#[test]
fn full_scan_builds_parent_chain_and_metadata() {
    let directory = TempDir::new().unwrap();
    let root = directory.path().join("volume");
    fs::create_dir_all(root.join("a/b")).unwrap();
    fs::write(root.join("a/b/file.txt"), b"hello").unwrap();
    fs::write(root.join("top.txt"), b"top").unwrap();
    let db_path = directory.path().join("index.sqlite");

    let outcome = index_root(&options(&db_path, &root)).unwrap();
    assert_eq!(outcome.stats.entries_total, 5);
    assert_eq!(count_in(&db_path, "SELECT COUNT(*) FROM files"), 5);

    let db = IndexDb::open(&db_path).unwrap();
    let root_path = canonical(&root);
    let root_id: String = db
        .connection()
        .query_row(
            "SELECT id FROM files WHERE path=?1",
            params![root_path],
            |row| row.get(0),
        )
        .unwrap();
    let root_parent: Option<String> = db
        .connection()
        .query_row(
            "SELECT parent_id FROM files WHERE id=?1",
            params![root_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(root_parent.is_none(), "volume root must have no parent");

    let file_path = canonical(&root.join("a/b/file.txt"));
    let (name, norm_name, extension, size, is_dir, parent_id, file_type): (
        String,
        String,
        String,
        i64,
        i64,
        String,
        String,
    ) = db
        .connection()
        .query_row(
            "SELECT name, norm_name, ext, size, is_dir, parent_id, file_type
             FROM files WHERE path=?1",
            params![file_path],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(name, "file.txt");
    assert_eq!(norm_name, "file.txt");
    assert_eq!(extension, "txt");
    assert_eq!(size, 5);
    assert_eq!(is_dir, 0);
    assert_eq!(file_type, "file");

    let b_id: String = db
        .connection()
        .query_row(
            "SELECT id FROM files WHERE path=?1",
            params![canonical(&root.join("a/b"))],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(parent_id, b_id, "file.txt must be parented by a/b");

    let a_parent: String = db
        .connection()
        .query_row(
            "SELECT parent_id FROM files WHERE path=?1",
            params![canonical(&root.join("a"))],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(a_parent, root_id);
    assert_eq!(outcome.volume.status.as_str(), "online");
}

#[test]
fn normalizes_nfc_names_in_storage() {
    let directory = TempDir::new().unwrap();
    let root = directory.path().join("volume");
    fs::create_dir_all(&root).unwrap();
    // Composed and decomposed forms are distinct directory entries on Linux.
    fs::write(root.join("caf\u{00e9}.txt"), b"a").unwrap();
    fs::write(root.join("cafe\u{0301}.txt"), b"b").unwrap();
    let db_path = directory.path().join("index.sqlite");

    index_root(&options(&db_path, &root)).unwrap();
    let db = IndexDb::open(&db_path).unwrap();
    let normalized = count(
        &db,
        "SELECT COUNT(*) FROM files WHERE norm_name='café.txt'",
        [],
    );
    assert_eq!(normalized, 2);
}

#[test]
fn commits_in_batches_of_5000() {
    let directory = TempDir::new().unwrap();
    let root = directory.path().join("volume");
    create_files(&root, 5001, "f");
    let db_path = directory.path().join("index.sqlite");

    let outcome = index_root(&options(&db_path, &root)).unwrap();
    // Root directory + 5001 files = 5002 entries => two 5000-entry transactions.
    assert_eq!(outcome.stats.entries_total, 5002);
    assert_eq!(outcome.stats.batches_committed, 2);
    assert_eq!(outcome.stats.inserted, 5002);
    assert_eq!(count_in(&db_path, "SELECT COUNT(*) FROM files"), 5002);

    let db = IndexDb::open(&db_path).unwrap();
    let (completed, total): (i64, i64) = db
        .connection()
        .query_row(
            "SELECT completed_batches, total_entries FROM scan_sessions ORDER BY rowid DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(completed, 2);
    assert_eq!(total, 5002);
}

#[test]
fn failed_batch_rolls_back_and_then_resumes() {
    let directory = TempDir::new().unwrap();
    let root = directory.path().join("volume");
    create_files(&root, 100, "f");
    let db_path = directory.path().join("index.sqlite");

    let failing = IndexOptions {
        batch_size: 50,
        fail_during_batch_after: Some(30),
        ..options(&db_path, &root)
    };
    let error = index_root(&failing).unwrap_err();
    assert!(matches!(error, IndexError::InjectedFailure { rows: 30 }));

    // The first batch was rolled back: nothing but the volume/session rows exist.
    assert_eq!(count_in(&db_path, "SELECT COUNT(*) FROM files"), 0);
    let running = count_in(
        &db_path,
        "SELECT COUNT(*) FROM scan_sessions WHERE status='running'",
    );
    assert_eq!(running, 1);

    let resumed = index_root(&options(&db_path, &root)).unwrap();
    assert!(resumed.stats.resumed);
    assert_eq!(resumed.stats.resumed_from_batch, 0);
    assert_eq!(resumed.stats.inserted, 101);
    assert_eq!(count_in(&db_path, "SELECT COUNT(*) FROM files"), 101);
}

#[test]
fn incremental_scan_updates_exactly_ten_files() {
    let directory = TempDir::new().unwrap();
    let root = directory.path().join("volume");
    let files = create_files(&root, 100, "f");
    let db_path = directory.path().join("index.sqlite");

    let first = index_root(&options(&db_path, &root)).unwrap();
    assert_eq!(first.stats.inserted, 101);

    for (index, path) in files.iter().take(10).enumerate() {
        fs::write(path, format!("modified-{index}").as_bytes()).unwrap();
    }

    let second = index_root(&options(&db_path, &root)).unwrap();
    assert_eq!(
        second.stats.updated, 10,
        "only the 10 modified files are written"
    );
    assert_eq!(second.stats.inserted, 0);
    assert_eq!(second.stats.marked_missing, 0);
    assert_eq!(second.stats.rows_written, 10);
    assert_eq!(second.stats.unchanged, 91);

    let db = IndexDb::open(&db_path).unwrap();
    let changed: i64 = count(
        &db,
        "SELECT COUNT(*) FROM files WHERE size > 1 AND file_type='file'",
        [],
    );
    assert_eq!(changed, 10);
}

#[test]
fn deletions_and_reappearance_use_missing_status() {
    let directory = TempDir::new().unwrap();
    let root = directory.path().join("volume");
    let files = create_files(&root, 100, "f");
    let db_path = directory.path().join("index.sqlite");

    index_root(&options(&db_path, &root)).unwrap();
    for path in files.iter().take(5) {
        fs::remove_file(path).unwrap();
    }

    let second = index_root(&options(&db_path, &root)).unwrap();
    assert_eq!(second.stats.marked_missing, 5);
    assert_eq!(second.stats.inserted, 0);
    assert_eq!(count_in(&db_path, "SELECT COUNT(*) FROM files"), 101);

    let db = IndexDb::open(&db_path).unwrap();
    let missing: i64 = count(&db, "SELECT COUNT(*) FROM files WHERE status='missing'", []);
    assert_eq!(missing, 5);
    let deleted_at_set: i64 = count(
        &db,
        "SELECT COUNT(*) FROM files WHERE status='missing' AND deleted_at IS NOT NULL",
        [],
    );
    assert_eq!(deleted_at_set, 5);

    // Recreating one file revives the same row instead of inserting a new one.
    fs::write(&files[0], b"back").unwrap();
    let third = index_root(&options(&db_path, &root)).unwrap();
    assert_eq!(third.stats.inserted, 0);
    assert!(
        third.stats.updated >= 1,
        "the revived file (and possibly its parent dir) must be rewritten"
    );
    let online: i64 = count(
        &db,
        "SELECT COUNT(*) FROM files WHERE path=?1 AND status='online'",
        params![canonical(&files[0])],
    );
    assert_eq!(online, 1);
}

#[test]
fn rename_is_missing_plus_insert() {
    let directory = TempDir::new().unwrap();
    let root = directory.path().join("volume");
    let files = create_files(&root, 10, "f");
    let db_path = directory.path().join("index.sqlite");

    index_root(&options(&db_path, &root)).unwrap();
    let old_path = files[0].clone();
    let old_path_string = canonical(&old_path);
    let new_path = root.join("renamed.txt");
    fs::rename(&old_path, &new_path).unwrap();

    let second = index_root(&options(&db_path, &root)).unwrap();
    assert_eq!(second.stats.inserted, 1);
    assert_eq!(second.stats.marked_missing, 1);
    // The parent directory mtime also changes on rename, so rows_written is
    // 3: one insert, one missing, one directory metadata update.
    assert_eq!(second.stats.rows_written, 3);

    let db = IndexDb::open(&db_path).unwrap();
    let old_status: String = db
        .connection()
        .query_row(
            "SELECT status FROM files WHERE path=?1",
            params![old_path_string],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old_status, "missing");
    let new_id: String = db
        .connection()
        .query_row(
            "SELECT id FROM files WHERE path=?1",
            params![canonical(&new_path)],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!new_id.is_empty());
}

#[test]
fn aborted_scan_resumes_from_checkpoint() {
    let directory = TempDir::new().unwrap();
    let root = directory.path().join("volume");
    create_files(&root, 300, "f");
    let db_path = directory.path().join("index.sqlite");

    let aborting = IndexOptions {
        batch_size: 100,
        abort_after_batches: Some(1),
        ..options(&db_path, &root)
    };
    let error = index_root(&aborting).unwrap_err();
    assert!(matches!(error, IndexError::Aborted { batches: 1 }));
    assert_eq!(count_in(&db_path, "SELECT COUNT(*) FROM files"), 100);

    let db = IndexDb::open(&db_path).unwrap();
    let (batches, offset, status): (i64, i64, String) = db
        .connection()
        .query_row(
            "SELECT completed_batches, work_file_offset, status FROM scan_sessions
             ORDER BY rowid DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(batches, 1);
    assert!(offset > 0);
    assert_eq!(status, "running");
    drop(db);

    let resumed = index_root(&options(&db_path, &root)).unwrap();
    assert!(resumed.stats.resumed);
    assert_eq!(resumed.stats.resumed_from_batch, 1);
    assert_eq!(resumed.stats.entries_total, 301);
    assert_eq!(resumed.stats.inserted, 201);
    assert_eq!(count_in(&db_path, "SELECT COUNT(*) FROM files"), 301);
    assert_eq!(
        count_in(&db_path, "SELECT COUNT(DISTINCT path) FROM files"),
        301
    );

    let db = IndexDb::open(&db_path).unwrap();
    let (status, work_file): (String, String) = db
        .connection()
        .query_row(
            "SELECT status, work_file FROM scan_sessions ORDER BY rowid DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "completed");
    assert!(
        !Path::new(&work_file).exists(),
        "completed sessions drop their crash-resume work file"
    );
}

#[test]
fn volume_offline_and_restore_keep_rows() {
    let directory = TempDir::new().unwrap();
    let root = directory.path().join("volume");
    create_files(&root, 20, "f");
    let db_path = directory.path().join("index.sqlite");

    index_root(&options(&db_path, &root)).unwrap();
    let total = count_in(&db_path, "SELECT COUNT(*) FROM files");

    let backup = directory.path().join("volume-unplugged");
    fs::rename(&root, &backup).unwrap();

    let mut db = IndexDb::open(&db_path).unwrap();
    let event = poll_volume(&mut db, &root).unwrap();
    match event {
        VolumeEvent::WentOffline {
            files_marked_offline,
            reason,
            ..
        } => {
            assert_eq!(files_marked_offline, total as u64);
            assert_eq!(reason, "mount point missing");
        }
        other => panic!("expected WentOffline, got {other:?}"),
    }
    assert_eq!(count_in(&db_path, "SELECT COUNT(*) FROM files"), total);
    assert_eq!(
        count_in(
            &db_path,
            "SELECT COUNT(*) FROM files WHERE status='offline'"
        ),
        total
    );
    let status: String = db
        .connection()
        .query_row("SELECT status FROM volumes LIMIT 1", [], |row| row.get(0))
        .unwrap();
    assert_eq!(status, "offline");

    fs::rename(&backup, &root).unwrap();
    let event = poll_volume(&mut db, &root).unwrap();
    match event {
        VolumeEvent::CameOnline { files_restored, .. } => {
            assert_eq!(files_restored, total as u64);
        }
        other => panic!("expected CameOnline, got {other:?}"),
    }
    assert_eq!(
        count_in(&db_path, "SELECT COUNT(*) FROM files WHERE status='online'"),
        total
    );
    assert_eq!(count_in(&db_path, "SELECT COUNT(*) FROM files"), total);
}

#[test]
fn unreadable_directory_does_not_mark_children_missing() {
    let directory = TempDir::new().unwrap();
    let root = directory.path().join("volume");
    let hidden = root.join("denied");
    create_files(&hidden, 3, "h");
    let db_path = directory.path().join("index.sqlite");

    index_root(&options(&db_path, &root)).unwrap();
    let before = count_in(&db_path, "SELECT COUNT(*) FROM files WHERE status='online'");

    let original = fs::metadata(&hidden).unwrap().permissions();
    fs::set_permissions(&hidden, fs::Permissions::from_mode(0o000)).unwrap();
    let second = index_root(&options(&db_path, &root)).unwrap();
    let after = count_in(&db_path, "SELECT COUNT(*) FROM files WHERE status='online'");
    fs::set_permissions(&hidden, original).unwrap();

    assert_eq!(second.stats.marked_missing, 0);
    assert_eq!(
        after, before,
        "rows under an unreadable directory stay online"
    );
}

#[test]
fn dispossessed_rows_are_never_physically_deleted() {
    let directory = TempDir::new().unwrap();
    let root = directory.path().join("volume");
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("sub/keep.txt"), b"k").unwrap();
    let db_path = directory.path().join("index.sqlite");

    index_root(&options(&db_path, &root)).unwrap();
    fs::remove_dir_all(root.join("sub")).unwrap();
    let second = index_root(&options(&db_path, &root)).unwrap();

    // The directory and its child are both kept, both marked missing.
    assert_eq!(second.stats.marked_missing, 1);
    assert_eq!(second.stats.cascaded_missing, 1);
    assert_eq!(count_in(&db_path, "SELECT COUNT(*) FROM files"), 3);
    let missing: i64 = count_in(
        &db_path,
        "SELECT COUNT(*) FROM files WHERE status='missing'",
    );
    assert_eq!(missing, 2);
    let online_sub: i64 = count_in(
        &db_path,
        "SELECT COUNT(*) FROM files WHERE status='online' AND path LIKE '%/sub%'",
    );
    assert_eq!(online_sub, 0);
}
