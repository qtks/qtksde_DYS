//! The M3 tables live in the shared index database, so the schema migration
//! in `walker-index` has to create them for existing v0.1.0 databases.

use std::path::Path;

use rusqlite::Connection;

fn create_m1_database(path: &Path) {
    let conn = Connection::open(path).expect("open fixture");
    conn.execute_batch(walker_index::SCHEMA_SQL)
        .expect("m1 schema");
    conn.execute(
        "INSERT INTO meta(key, value) VALUES ('schema_version', 'panopticon.m1.index.v1')",
        [],
    )
    .expect("meta row");
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
fn semantic_tables_are_queryable_after_migrating_an_m1_database() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("index.db");
    create_m1_database(&path);

    walker_index::IndexDb::open(&path).expect("migrate m1 database");

    let conn = walker_ai::db::open_ro(&path).expect("open read-only");
    for table in [
        "projects",
        "vectors",
        "vector_shards",
        "archive_suggestions",
    ] {
        assert!(
            object_exists(&conn, "table", table),
            "migration did not create {table}"
        );
    }
    assert!(object_exists(&conn, "index", "idx_vectors_shard"));

    // Schema-level check: the queries the semantic/suggestion code runs on an
    // empty index must succeed (the embedding runtime is not needed for this).
    let vectors: i64 = conn
        .query_row("SELECT COUNT(*) FROM vectors", [], |row| row.get(0))
        .expect("count vectors");
    assert_eq!(vectors, 0);
    let shards: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM vector_shards WHERE model IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .expect("count shards");
    assert_eq!(shards, 0);
    let pending: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM archive_suggestions WHERE status='pending'",
            [],
            |row| row.get(0),
        )
        .expect("count suggestions");
    assert_eq!(pending, 0);
}
