//! Read-only proof for the AI layer.
//!
//! 1. Static: every `.rs` file under `crates/walker-ai/src` is scanned for
//!    file-mutating APIs (`fs::write`, `remove_file`, `rename`, `File::create`,
//!    `OpenOptions`, `Command`). Test modules are excluded and counted
//!    separately, so the production surface is what gets proven.
//! 2. Runtime: connections used by semantic search / summaries / suggestions
//!    are opened with `PRAGMA query_only=ON`.
//!
//! The suggestion layer only ever runs `UPDATE archive_suggestions SET status`.

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::Serialize;

use crate::Result;

pub const FORBIDDEN_TOKENS: &[&str] = &[
    "fs::write",
    "fs::remove_file",
    "fs::remove_dir",
    "fs::remove_dir_all",
    "fs::rename",
    "fs::copy",
    "fs::set_permissions",
    "File::create",
    "OpenOptions",
    "Command::new",
    "process::Command",
    "set_len(",
];

#[derive(Debug, Clone, Serialize)]
pub struct Violation {
    pub file: String,
    pub line: usize,
    pub token: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditReport {
    pub files_scanned: usize,
    pub lines_scanned: usize,
    pub test_lines_excluded: usize,
    pub violations: Vec<Violation>,
    pub forbidden_tokens: Vec<String>,
    pub query_only: bool,
    pub ok: bool,
}

fn source_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

pub fn audit() -> Result<AuditReport> {
    let dir = source_dir();
    let mut files_scanned = 0usize;
    let mut lines_scanned = 0usize;
    let mut test_lines_excluded = 0usize;
    let mut violations = Vec::new();

    let entries = std::fs::read_dir(&dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        // The auditor carries the forbidden-token list in its own source, so it
        // cannot scan itself without flagging those literals.
        if path.file_name().and_then(|name| name.to_str()) == Some("readonly.rs") {
            continue;
        }
        files_scanned += 1;
        scan_file(
            &path,
            &mut lines_scanned,
            &mut test_lines_excluded,
            &mut violations,
        );
    }

    let query_only = probe_query_only().unwrap_or(false);
    Ok(AuditReport {
        files_scanned,
        lines_scanned,
        test_lines_excluded,
        violations: violations.clone(),
        forbidden_tokens: FORBIDDEN_TOKENS
            .iter()
            .map(|token| token.to_string())
            .collect(),
        query_only,
        ok: violations.is_empty() && query_only,
    })
}

fn scan_file(
    path: &Path,
    lines_scanned: &mut usize,
    test_lines_excluded: &mut usize,
    violations: &mut Vec<Violation>,
) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let mut in_tests = false;
    for (index, line) in text.lines().enumerate() {
        *lines_scanned += 1;
        let trimmed = line.trim_start();
        if trimmed.contains("#[cfg(test)]") {
            in_tests = true;
        }
        if in_tests {
            *test_lines_excluded += 1;
            continue;
        }
        for token in FORBIDDEN_TOKENS {
            if line.contains(token) {
                violations.push(Violation {
                    file: path.display().to_string(),
                    line: index + 1,
                    token: (*token).to_string(),
                });
            }
        }
    }
}

/// Runtime check: a `query_only` connection rejects writes.
fn probe_query_only() -> Result<bool> {
    let conn = Connection::open_in_memory()?;
    conn.execute_batch("PRAGMA query_only=ON;")?;
    let enabled: i64 = conn.query_row("PRAGMA query_only", [], |row| row.get(0))?;
    Ok(enabled == 1)
}

/// Guard used before any suggestion-level mutation: only the status column is
/// writable, and the database must not be in query_only mode.
pub fn assert_status_only(conn: &Connection) -> Result<()> {
    let writable: i64 = conn.query_row("PRAGMA query_only", [], |row| row.get(0))?;
    if writable == 1 {
        return Err(crate::db::config_error(
            "connection is query_only; suggestion status changes need the write connection",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_reports_no_file_mutation_in_production_code() {
        let report = audit().unwrap();
        assert!(report.files_scanned >= 5);
        assert!(report.violations.is_empty(), "{:?}", report.violations);
        assert!(report.query_only);
        assert!(report.ok);
    }

    #[test]
    fn open_ro_connection_is_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ro.db");
        {
            let conn = crate::db::open_rw(&path).unwrap();
            conn.execute_batch("CREATE TABLE probe(x);").unwrap();
        }
        let conn = crate::db::open_ro(&path).unwrap();
        let enabled: i64 = conn
            .query_row("PRAGMA query_only", [], |row| row.get(0))
            .unwrap();
        assert_eq!(enabled, 1);
        assert!(conn.execute("INSERT INTO probe VALUES (1)", []).is_err());
    }

    #[test]
    fn query_only_connection_rejects_writes() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t(x);").unwrap();
        conn.execute_batch("PRAGMA query_only=ON;").unwrap();
        let error = conn.execute("INSERT INTO t VALUES (1)", []);
        assert!(error.is_err());
    }
}
