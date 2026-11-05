//! Migration tests: an M1 database written by v0.1.0 must keep working.

use std::path::Path;

use rusqlite::Connection;
use walker_index::{IndexDb, SCHEMA_SQL, SCHEMA_VERSION};

/// Build the v0.1.0 (M1) database: base tables only, `user_version` still 0.
fn create_m1_database(path: &Path) {
    let conn = Connection::open(path).expect("open fixture");
    conn.execute_batch(SCHEMA_SQL).expect("m1 schema");
    conn.execute(
        "INSERT INTO meta(key, value) VALUES ('schema_version', 'panopticon.m1.index.v1')",
        [],
    )
    .expect("meta row");
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user_version");
    assert_eq!(
        version, 0,
        "the fixture must look like an un-migrated v0.1.0 db"
    );
}

fn object_exists(conn: &Connection, kind: &str, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type=?1 AND name=?2",
        [kind, name],
        |row| row.get::<_, i64>(0),
    )
    .expect("sqlite_master")
        > 0
}

#[test]
fn opening_an_m1_database_applies_all_migrations() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("index.db");
    create_m1_database(&path);

    let db = IndexDb::open(&path).expect("migrate m1 database");

    let version: i64 = db
        .connection()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user_version");
    assert_eq!(version as u32, SCHEMA_VERSION);

    for table in [
        "dir_totals",
        "projects",
        "vectors",
        "vector_shards",
        "archive_suggestions",
    ] {
        assert!(
            object_exists(db.connection(), "table", table),
            "migration did not create {table}"
        );
    }
    for index in [
        "idx_dir_totals_path",
        "idx_vectors_shard",
        "idx_suggestions_status",
    ] {
        assert!(
            object_exists(db.connection(), "index", index),
            "migration did not create {index}"
        );
    }

    // Idempotent: opening an already migrated database is a no-op.
    drop(db);
    IndexDb::open(&path).expect("re-open migrated database");
}

#[test]
fn refuses_a_database_from_a_newer_app_with_a_readable_error() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("index.db");
    create_m1_database(&path);
    Connection::open(&path)
        .expect("open fixture")
        .execute_batch(&format!("PRAGMA user_version={};", SCHEMA_VERSION + 1))
        .expect("bump version");

    let error = IndexDb::open(&path)
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default();
    assert!(
        error.contains("newer than supported"),
        "expected a readable upgrade hint, got: {error}"
    );
}

#[test]
fn reports_which_migration_step_failed() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("index.db");
    create_m1_database(&path);
    {
        // Occupying the index name makes the v2 step fail.
        let conn = Connection::open(&path).expect("open fixture");
        conn.execute("CREATE TABLE idx_dir_totals_path (blocker INTEGER)", [])
            .expect("blocker table");
        conn.execute_batch("PRAGMA user_version=1;")
            .expect("set v1");
    }

    let error = IndexDb::open(&path)
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default();
    assert!(
        error.contains("schema migration to v2 failed"),
        "expected the failing version in the message, got: {error}"
    );
}
