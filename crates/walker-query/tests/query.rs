use std::fs;
use std::io::Write;

use rusqlite::params;
use walker_index::IndexDb;
use walker_query::{
    aggregate_dir_totals, child_rows, ensure_dir_totals, ensure_fts, find_duplicates, fts_status,
    rebuild_fts, search, treemap_rows, DuplicateOptions, PrefixMode, SearchKind, SearchOptions,
};

fn setup() -> IndexDb {
    let db = IndexDb::open_in_memory().expect("schema");
    db.connection()
        .execute(
            "INSERT INTO volumes (id, name, mount_path, device, status, last_seen_at, created_at, updated_at)
             VALUES ('v1', 'v1', '/mnt/v1', NULL, 'online', '2026-01-01', '2026-01-01', '2026-01-01')",
            [],
        )
        .expect("volume");
    db.connection()
        .execute(
            "INSERT INTO volumes (id, name, mount_path, device, status, last_seen_at, created_at, updated_at)
             VALUES ('v2', 'v2', '/mnt/v2', NULL, 'offline', '2026-01-01', '2026-01-01', '2026-01-01')",
            [],
        )
        .expect("volume");
    db
}

#[allow(clippy::too_many_arguments)]
fn insert_file(
    db: &IndexDb,
    id: &str,
    volume: &str,
    parent: Option<&str>,
    name: &str,
    path: &str,
    is_dir: bool,
    size: i64,
    inode: i64,
    status: &str,
    content_hash: Option<&str>,
) {
    db.connection()
        .execute(
            "INSERT INTO files (id, volume_id, parent_id, name, norm_name, path, is_dir, ext, size,
                                mtime, inode, file_type, content_hash, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, NULL, ?7, 0, ?8, 'file', ?9, ?10,
                     '2026-01-01', '2026-01-01')",
            params![
                id,
                volume,
                parent,
                name,
                path,
                is_dir as i64,
                size,
                inode,
                content_hash,
                status
            ],
        )
        .expect("file row");
}

#[test]
fn search_exact_prefix_and_nfc() {
    let db = setup();
    insert_file(
        &db, "root", "v1", None, "v1", "/mnt/v1", true, 0, 1, "online", None,
    );
    // NFC vs NFD: both normalize to the same query key.
    insert_file(
        &db,
        "nfc",
        "v1",
        Some("root"),
        "caf\u{e9}.txt",
        "/mnt/v1/caf\u{e9}.txt",
        false,
        10,
        2,
        "online",
        None,
    );
    insert_file(
        &db,
        "plain",
        "v1",
        Some("root"),
        "alpha.txt",
        "/mnt/v1/alpha.txt",
        false,
        20,
        3,
        "online",
        None,
    );
    insert_file(
        &db,
        "offline",
        "v1",
        Some("root"),
        "alpha-off.txt",
        "/mnt/v1/alpha-off.txt",
        false,
        30,
        4,
        "offline",
        None,
    );

    let conn = db.connection();
    let exact = search(
        conn,
        &SearchOptions {
            kind: SearchKind::Exact,
            query: "cafe\u{301}.txt".to_string(), // NFD input
            limit: 10,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(exact.strategy, "exact_index");
    assert_eq!(exact.hits.len(), 1);
    assert_eq!(exact.hits[0].name, "caf\u{e9}.txt");

    for mode in [PrefixMode::Range, PrefixMode::Like] {
        let prefix = search(
            conn,
            &SearchOptions {
                kind: SearchKind::Prefix,
                query: "alpha".to_string(),
                limit: 10,
                prefix_mode: mode,
                ..Default::default()
            },
        )
        .unwrap();
        // Offline rows are excluded from search results.
        assert_eq!(prefix.hits.len(), 1, "mode: {mode:?}");
        assert_eq!(prefix.hits[0].name, "alpha.txt");
        assert!(
            prefix
                .plan
                .iter()
                .any(|line| line.contains("idx_files_norm_name")),
            "expected index use, plan: {:?}",
            prefix.plan
        );
    }
}

#[test]
fn fts_trigram_and_short_query_fallback() {
    let db = setup();
    insert_file(
        &db, "root", "v1", None, "v1", "/mnt/v1", true, 0, 1, "online", None,
    );
    insert_file(
        &db,
        "a",
        "v1",
        Some("root"),
        "会议纪要设计文档.pdf",
        "/mnt/v1/会议纪要设计文档.pdf",
        false,
        100,
        10,
        "online",
        None,
    );
    insert_file(
        &db,
        "b",
        "v1",
        Some("root"),
        "年度预算表.xlsx",
        "/mnt/v1/年度预算表.xlsx",
        false,
        200,
        11,
        "online",
        None,
    );

    let conn = db.connection();
    let report = rebuild_fts(conn).unwrap();
    // root dir + two files
    assert_eq!(report.rows, 3);

    // >= 3 chars uses the trigram index.
    let hit = search(
        conn,
        &SearchOptions {
            kind: SearchKind::Fts,
            query: "设计文".to_string(),
            limit: 10,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(hit.strategy, "fts_trigram_unranked");
    assert_eq!(hit.hits.len(), 1);

    // < 3 chars falls back to LIKE and says so.
    let short = search(
        conn,
        &SearchOptions {
            kind: SearchKind::Fts,
            query: "预算".to_string(),
            limit: 10,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(short.strategy, "fts_like_fallback");
    assert_eq!(short.hits.len(), 1);
    assert!(short.note.is_some());

    // FTS stays stale (not silently wrong) after a row is added.
    insert_file(
        &db,
        "c",
        "v1",
        Some("root"),
        "预算补充.docx",
        "/mnt/v1/预算补充.docx",
        false,
        300,
        12,
        "online",
        None,
    );
    let status = fts_status(conn).unwrap();
    assert!(status.stale);
    assert_eq!(status.indexed_rows, 3);
    assert_eq!(status.online_rows, 4);
}

/// Startup builds the FTS index when it is missing, then leaves it alone.
#[test]
fn ensure_fts_builds_the_index_on_first_use() {
    let db = setup();
    insert_file(
        &db, "root", "v1", None, "v1", "/mnt/v1", true, 0, 1, "online", None,
    );
    insert_file(
        &db,
        "cn",
        "v1",
        Some("root"),
        "年度报告-2026.txt",
        "/mnt/v1/年度报告-2026.txt",
        false,
        10,
        2,
        "online",
        None,
    );
    let conn = db.connection();

    assert!(!fts_status(conn).unwrap().table_exists);
    let report = ensure_fts(conn).unwrap().expect("index was missing");
    assert_eq!(report.rows, 2);
    assert!(!fts_status(conn).unwrap().stale);
    assert!(ensure_fts(conn).unwrap().is_none());

    // The Chinese query is served by the trigram index, not the LIKE fallback.
    let hit = search(
        conn,
        &SearchOptions {
            kind: SearchKind::Fts,
            query: "年度报告".to_string(),
            limit: 10,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(hit.strategy, "fts_trigram_unranked");
    assert_eq!(hit.hits.len(), 1);
}

#[test]
fn duplicate_funnel_skips_hardlinks_offline_and_empty() {
    let dir = tempfile::tempdir().unwrap();
    let payload = b"same-content";
    let make = |name: &str, contents: &[u8]| {
        let path = dir.path().join(name);
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(contents).unwrap();
        path
    };
    // Two byte-identical files, one different file of the same size.
    let first = make("a.bin", payload);
    let second = make("b.bin", payload);
    let different = make("c.bin", b"same-contenX");
    let empty = make("empty.bin", b"");
    fs::write(dir.path().join("empty2.bin"), b"").unwrap();
    fs::hard_link(&first, dir.path().join("a-hl.bin")).unwrap();

    let db = setup();
    for (index, path) in [&first, &second, &different].iter().enumerate() {
        insert_file(
            &db,
            &format!("f{index}"),
            "v1",
            None,
            path.file_name().unwrap().to_str().unwrap(),
            path.to_str().unwrap(),
            false,
            payload.len() as i64,
            (index + 1) as i64,
            "online",
            None,
        );
    }
    // Hardlink row: same inode as f0.
    insert_file(
        &db,
        "f3",
        "v1",
        None,
        "a-hl.bin",
        "/tmp/a-hl.bin",
        false,
        payload.len() as i64,
        1,
        "online",
        None,
    );
    // Zero-byte files are skipped.
    insert_file(
        &db,
        "f4",
        "v1",
        None,
        "empty.bin",
        "/tmp/empty.bin",
        false,
        0,
        5,
        "online",
        None,
    );
    // Offline volume row with the same content must not be counted.
    insert_file(
        &db,
        "f5",
        "v2",
        None,
        "offline.bin",
        "/mnt/v2/offline.bin",
        false,
        payload.len() as i64,
        6,
        "offline",
        None,
    );

    let report = find_duplicates(db.connection(), &DuplicateOptions::default()).unwrap();
    assert_eq!(report.duplicate_groups, 1);
    assert_eq!(report.duplicate_files, 1);
    assert_eq!(report.reclaimable_bytes, payload.len() as u64);
    assert_eq!(report.skipped_hardlink_rows, 1);
    assert_eq!(report.skipped_empty_files, 1);
    assert_eq!(
        report.full_reused, 2,
        "small files reuse their whole-file hash"
    );
    assert!(report.groups[0]
        .files
        .iter()
        .all(|file| file.volume_id == "v1"));
    let _ = different;
    let _ = empty;
}

#[test]
fn aggregate_and_treemap_sort_by_subtree_bytes() {
    let db = setup();
    insert_file(
        &db, "root", "v1", None, "v1", "/mnt/v1", true, 0, 1, "online", None,
    );
    insert_file(
        &db,
        "d1",
        "v1",
        Some("root"),
        "a",
        "/mnt/v1/a",
        true,
        0,
        2,
        "online",
        None,
    );
    insert_file(
        &db,
        "d2",
        "v1",
        Some("root"),
        "b",
        "/mnt/v1/b",
        true,
        0,
        3,
        "online",
        None,
    );
    insert_file(
        &db,
        "d3",
        "v1",
        Some("d1"),
        "c",
        "/mnt/v1/a/c",
        true,
        0,
        4,
        "online",
        None,
    );
    insert_file(
        &db,
        "f1",
        "v1",
        Some("d3"),
        "one",
        "/mnt/v1/a/c/one",
        false,
        100,
        5,
        "online",
        None,
    );
    insert_file(
        &db,
        "f2",
        "v1",
        Some("d1"),
        "two",
        "/mnt/v1/a/two",
        false,
        50,
        6,
        "online",
        None,
    );
    insert_file(
        &db,
        "f3",
        "v1",
        Some("d2"),
        "three",
        "/mnt/v1/b/three",
        false,
        30,
        7,
        "online",
        None,
    );
    insert_file(
        &db,
        "f4",
        "v1",
        Some("root"),
        "rootfile",
        "/mnt/v1/rootfile",
        false,
        10,
        8,
        "online",
        None,
    );

    let report = aggregate_dir_totals(db.connection()).unwrap();
    assert_eq!(report.directories, 4);
    assert_eq!(report.online_files, 4);
    assert_eq!(report.bytes_indexed, 190);

    let root = treemap_rows(db.connection(), Some("/mnt/v1"), 10).unwrap();
    assert_eq!(root.bytes, 190);
    assert_eq!(root.children.len(), 3);
    assert_eq!(root.children[0].name, "a");
    assert_eq!(root.children[0].bytes, 150);
    assert_eq!(root.children[1].name, "b");
    assert_eq!(root.children[1].bytes, 30);

    let page = child_rows(db.connection(), "/mnt/v1", 10, 0, "name").unwrap();
    assert_eq!(page.children.len(), 3);
    assert!(!page.truncated);
}

/// Opening a v0.1.0 (M1) database must leave the treemap usable: the
/// aggregate step writes into `dir_totals`, which M1 did not have.
#[test]
fn treemap_works_after_migrating_an_m1_database() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("index.db");
    {
        let conn = rusqlite::Connection::open(&path).expect("fixture");
        conn.execute_batch(walker_index::SCHEMA_SQL)
            .expect("m1 schema");
        conn.execute(
            "INSERT INTO meta(key, value) VALUES ('schema_version', 'panopticon.m1.index.v1')",
            [],
        )
        .expect("meta row");
    }

    let db = IndexDb::open(&path).expect("migrate m1 database");
    db.connection()
        .execute_batch(
            r#"
            INSERT INTO volumes (id, name, mount_path, device, status, last_seen_at, created_at, updated_at)
            VALUES ('v1', 'v1', '/mnt/v1', NULL, 'online', '2026-01-01', '2026-01-01', '2026-01-01');
            INSERT INTO files (id, volume_id, parent_id, name, norm_name, path, is_dir, size, file_type,
                               created_at, updated_at)
            VALUES ('root', 'v1', NULL, 'v1', 'v1', '/mnt/v1', 1, 0, 'dir', '2026-01-01', '2026-01-01'),
                   ('sub', 'v1', 'root', 'sub', 'sub', '/mnt/v1/sub', 1, 0, 'dir', '2026-01-01', '2026-01-01'),
                   ('f1', 'v1', 'sub', 'f.bin', 'f.bin', '/mnt/v1/sub/f.bin', 0, 100, 'file',
                    '2026-01-01', '2026-01-01');
            "#,
        )
        .expect("rows");

    aggregate_dir_totals(db.connection()).expect("aggregate after migration");
    let root = treemap_rows(db.connection(), Some("/mnt/v1"), 10).expect("treemap after migration");
    assert_eq!(root.bytes, 100);
    assert!(root
        .children
        .iter()
        .any(|child| child.path == "/mnt/v1/sub" && child.bytes == 100));
}

/// The server rebuilds `dir_totals` at startup when a CLI scan left it empty,
/// and does not rebuild it again while the index is unchanged.
#[test]
fn ensure_dir_totals_is_automatic_and_idempotent() {
    let db = setup();
    insert_file(
        &db, "root", "v1", None, "v1", "/mnt/v1", true, 0, 1, "online", None,
    );
    insert_file(
        &db,
        "f1",
        "v1",
        Some("root"),
        "a.bin",
        "/mnt/v1/a.bin",
        false,
        40,
        2,
        "online",
        None,
    );

    let report = ensure_dir_totals(db.connection())
        .expect("first build")
        .expect("totals were missing");
    assert_eq!(report.directories, 1);
    assert_eq!(report.bytes_indexed, 40);
    assert!(ensure_dir_totals(db.connection())
        .expect("second pass")
        .is_none());

    // A later scan adds rows; the stored row count no longer matches.
    insert_file(
        &db,
        "f2",
        "v1",
        Some("root"),
        "b.bin",
        "/mnt/v1/b.bin",
        false,
        60,
        3,
        "online",
        None,
    );
    let report = ensure_dir_totals(db.connection())
        .expect("rebuild after scan")
        .expect("totals were stale");
    assert_eq!(report.bytes_indexed, 100);
    assert!(ensure_dir_totals(db.connection())
        .expect("third pass")
        .is_none());
}
