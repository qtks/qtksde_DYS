//! Archive suggestions.
//!
//! Suggestions are *proposals* stored in `archive_suggestions`. Approving or
//! rejecting one only flips its `status` column; no code path in this module
//! touches, moves, or deletes a user file.

use std::collections::{BTreeMap, HashSet};

use rusqlite::{params, Connection};
use serde::Serialize;

use crate::db::{ensure_schema, now_iso};
use crate::Result;

#[derive(Debug, Clone, Serialize)]
pub struct Suggestion {
    pub id: String,
    pub kind: String,
    pub action: String,
    pub paths: Vec<String>,
    pub evidence: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SuggestReport {
    pub generated: u64,
    pub kept: u64,
    pub kinds: Vec<String>,
    pub min_cluster: usize,
}

/// Rule 1: projects of one inferred kind spread over >= `min_cluster` distinct
/// parent directories ("scattered siblings"), e.g. several KiCad projects that
/// do not sit next to each other.
/// Rule 2: content-hash duplicate clusters whose members live under different
/// top-level roots (only possible when M1 stored hashes).
pub fn generate(conn: &mut Connection, min_cluster: usize, limit: usize) -> Result<SuggestReport> {
    ensure_schema(conn)?;
    let started_at = now_iso(conn);
    let min_cluster = min_cluster.max(2);
    let mut generated = 0u64;
    let mut kept = 0u64;
    let mut kinds: Vec<String> = Vec::new();

    let mut projects_by_kind: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    {
        let mut statement =
            conn.prepare("SELECT kind, root_path, id FROM projects ORDER BY kind, root_path")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let kind: String = row.get(0)?;
            let root: String = row.get(1)?;
            let id: String = row.get(2)?;
            projects_by_kind.entry(kind).or_default().push((root, id));
        }
    }

    for (kind, projects) in &projects_by_kind {
        if projects.len() < min_cluster {
            continue;
        }
        let mut parents: HashSet<String> = HashSet::new();
        for (root, _) in projects {
            let parent = root
                .rsplit_once('/')
                .map(|(parent, _)| parent.to_string())
                .unwrap_or_default();
            parents.insert(parent);
        }
        if parents.len() < min_cluster {
            // Adjacent siblings: they already belong together, no suggestion.
            continue;
        }
        let paths: Vec<String> = projects.iter().map(|(root, _)| root.clone()).collect();
        let evidence = format!(
            "{kind} 工程 {} 个，分布在 {} 个互不相邻的父目录：{}",
            paths.len(),
            parents.len(),
            paths.join("、")
        );
        let id = stable_id("scattered-projects", &paths);
        let action = "考虑把这些同类工程归档到同一父目录（人工确认后执行）";
        upsert(
            conn,
            Draft {
                id: &id,
                kind: "scattered-projects",
                action,
                paths: &paths,
                evidence: &evidence,
                timestamp: &started_at,
            },
            &mut kept,
        )?;
        generated += 1;
        kinds.push(format!("scattered-projects/{kind}"));
    }

    // Rule 2: duplicate clusters spanning distinct roots.
    let mut hash_groups: Vec<(String, Vec<String>)> = Vec::new();
    {
        let mut statement = conn.prepare(
            "SELECT content_hash, path FROM files
             WHERE status='online' AND is_dir=0 AND content_hash IS NOT NULL
             ORDER BY content_hash",
        )?;
        let mut rows = statement.query([])?;
        let mut current: Option<(String, Vec<String>)> = None;
        while let Some(row) = rows.next()? {
            let hash: String = row.get(0)?;
            let path: String = row.get(1)?;
            match current.take() {
                Some((current_hash, mut paths)) if current_hash == hash => {
                    paths.push(path);
                    current = Some((hash, paths));
                }
                Some((current_hash, paths)) => {
                    if paths.len() > 1 {
                        hash_groups.push((current_hash, paths));
                    }
                    current = Some((hash, vec![path]));
                }
                None => current = Some((hash, vec![path])),
            }
        }
        if let Some((hash, paths)) = current {
            if paths.len() > 1 {
                hash_groups.push((hash, paths));
            }
        }
    }
    for (hash, paths) in hash_groups {
        if generated as usize >= limit {
            break;
        }
        let roots: HashSet<String> = paths
            .iter()
            .filter_map(|path| path.split('/').nth(1).map(|root| root.to_string()))
            .collect();
        if roots.len() < 2 {
            continue;
        }
        let evidence = format!(
            "内容哈希 {} 命中 {} 个文件，分布在 {} 个不同顶层目录",
            &hash[..hash.len().min(16)],
            paths.len(),
            roots.len()
        );
        let id = stable_id("duplicate-cluster", &paths);
        upsert(
            conn,
            Draft {
                id: &id,
                kind: "duplicate-cluster",
                action: "人工复核后合并或删除冗余副本（工具不会执行删除）",
                paths: &paths,
                evidence: &evidence,
                timestamp: &started_at,
            },
            &mut kept,
        )?;
        generated += 1;
        kinds.push("duplicate-cluster".to_string());
    }

    Ok(SuggestReport {
        generated,
        kept,
        kinds,
        min_cluster,
    })
}

struct Draft<'a> {
    id: &'a str,
    kind: &'a str,
    action: &'a str,
    paths: &'a [String],
    evidence: &'a str,
    timestamp: &'a str,
}

fn upsert(conn: &Connection, draft: Draft<'_>, kept: &mut u64) -> Result<()> {
    let paths_json = serde_json::to_string(draft.paths).unwrap_or_else(|_| "[]".to_string());
    let changed = conn.execute(
        "INSERT INTO archive_suggestions (id, kind, action, paths, evidence, status, created_at, updated_at)
         VALUES (?1,?2,?3,?4,?5,'pending',?6,?6)
         ON CONFLICT(id) DO UPDATE SET evidence=excluded.evidence, updated_at=excluded.updated_at
         WHERE archive_suggestions.status='pending'",
        params![
            draft.id,
            draft.kind,
            draft.action,
            paths_json,
            draft.evidence,
            draft.timestamp
        ],
    )?;
    if changed == 0 {
        *kept += 1;
    }
    Ok(())
}

fn stable_id(kind: &str, paths: &[String]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(kind.as_bytes());
    for path in paths {
        hasher.update(b"\0");
        hasher.update(path.as_bytes());
    }
    format!("sug_{}", &hasher.finalize().to_hex().to_string()[..16])
}

pub fn list(conn: &Connection, status: Option<&str>) -> Result<Vec<Suggestion>> {
    let mut statement = conn.prepare(
        "SELECT id, kind, action, paths, evidence, status, created_at, updated_at
         FROM archive_suggestions WHERE (?1 IS NULL OR status=?1)
         ORDER BY created_at DESC, id",
    )?;
    let mut rows = statement.query(params![status])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let paths: String = row.get(3)?;
        out.push(Suggestion {
            id: row.get(0)?,
            kind: row.get(1)?,
            action: row.get(2)?,
            paths: serde_json::from_str(&paths).unwrap_or_default(),
            evidence: row.get(4)?,
            status: row.get(5)?,
            created_at: row.get(6)?,
            updated_at: row.get(7)?,
        });
    }
    Ok(out)
}

/// The only mutation this crate performs on a suggestion: flip the status.
/// No file operation follows, by design.
pub fn set_status(conn: &mut Connection, id: &str, status: &str) -> Result<bool> {
    if status != "approved" && status != "rejected" && status != "pending" {
        return Err(crate::db::config_error(format!(
            "invalid status {status}; expected pending/approved/rejected"
        )));
    }
    ensure_schema(conn)?;
    let timestamp = now_iso(conn);
    let changed = conn.execute(
        "UPDATE archive_suggestions SET status=?2, updated_at=?3 WHERE id=?1",
        params![id, status, timestamp],
    )?;
    Ok(changed > 0)
}
