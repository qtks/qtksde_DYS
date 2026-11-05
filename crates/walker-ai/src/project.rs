//! Project detector: marker directories, inheritance, and incremental rebuild.

use std::collections::{BTreeMap, HashSet};
use std::time::Instant;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::db::{ensure_schema, now_iso};
use crate::Result;

/// Directories that are never project roots themselves: dependency trees and
/// VCS internals contain markers (`package.json`, `Makefile`, ...) by accident.
pub const SUPPRESSED_COMPONENTS: [&str; 3] = ["node_modules", "vendor", ".git"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Marker {
    Git,
    PlatformIo,
    Kicad,
    Ioc,
    Makefile,
    CMake,
    PackageJson,
    Sln,
}

impl Marker {
    pub fn label(self) -> &'static str {
        match self {
            Marker::Git => ".git",
            Marker::PlatformIo => "platformio.ini",
            Marker::Kicad => "*.kicad_pro",
            Marker::Ioc => "*.ioc",
            Marker::Makefile => "Makefile",
            Marker::CMake => "CMakeLists.txt",
            Marker::PackageJson => "package.json",
            Marker::Sln => "*.sln",
        }
    }

    fn detect(name: &str, ext: Option<&str>, is_dir: bool) -> Option<Self> {
        if is_dir {
            return (name == ".git").then_some(Marker::Git);
        }
        match (name, ext) {
            ("platformio.ini", _) => Some(Marker::PlatformIo),
            ("Makefile", _) => Some(Marker::Makefile),
            ("CMakeLists.txt", _) => Some(Marker::CMake),
            ("package.json", _) => Some(Marker::PackageJson),
            (_, Some("kicad_pro")) => Some(Marker::Kicad),
            (_, Some("ioc")) => Some(Marker::Ioc),
            (_, Some("sln")) => Some(Marker::Sln),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectKind {
    Kicad,
    Firmware,
    DotNet,
    Node,
    CMake,
    Make,
    Generic,
}

impl ProjectKind {
    pub fn infer(markers: &BTreeMap<Marker, usize>) -> Self {
        if markers.contains_key(&Marker::Kicad) {
            ProjectKind::Kicad
        } else if markers.contains_key(&Marker::PlatformIo) || markers.contains_key(&Marker::Ioc) {
            ProjectKind::Firmware
        } else if markers.contains_key(&Marker::Sln) {
            ProjectKind::DotNet
        } else if markers.contains_key(&Marker::PackageJson) {
            ProjectKind::Node
        } else if markers.contains_key(&Marker::CMake) {
            ProjectKind::CMake
        } else if markers.contains_key(&Marker::Makefile) {
            ProjectKind::Make
        } else {
            ProjectKind::Generic
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ProjectKind::Kicad => "kicad",
            ProjectKind::Firmware => "firmware",
            ProjectKind::DotNet => "dotnet",
            ProjectKind::Node => "node",
            ProjectKind::CMake => "cmake",
            ProjectKind::Make => "make",
            ProjectKind::Generic => "generic",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub volume_id: String,
    pub root_path: String,
    pub kind: ProjectKind,
    pub markers: Vec<String>,
    pub depth: usize,
    pub file_count: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebuildMode {
    /// Clear every previous assignment first. Use after a schema reset.
    Full,
    /// Only touch subtrees whose marker set changed, plus rows that have no
    /// assignment yet. This is what an incremental rescan should call.
    Incremental,
}

impl RebuildMode {
    pub fn as_str(self) -> &'static str {
        match self {
            RebuildMode::Full => "full",
            RebuildMode::Incremental => "incremental",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectReport {
    pub mode: &'static str,
    pub roots: usize,
    pub assigned_rows: u64,
    pub cleared_rows: u64,
    pub suppressed_marker_dirs: usize,
    pub took_ms: u64,
    pub projects: Vec<Project>,
}

fn is_suppressed(path: &str) -> bool {
    path.split(['/', '\\'])
        .any(|component| SUPPRESSED_COMPONENTS.contains(&component))
}

fn parent_dir(path: &str) -> Option<&str> {
    let index = path.rfind(['/', '\\'])?;
    Some(&path[..index])
}

fn depth_of(path: &str) -> usize {
    path.split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .count()
}

fn project_id(volume_id: &str, root_path: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(volume_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(root_path.as_bytes());
    format!("prj_{}", &hasher.finalize().to_hex().as_str()[..16])
}

struct Candidate {
    volume_id: String,
    root_path: String,
    markers: BTreeMap<Marker, usize>,
    depth: usize,
}

fn collect_candidates(conn: &Connection, volume: Option<&str>) -> Result<(Vec<Candidate>, usize)> {
    let volume_clause = if volume.is_some() {
        " AND f.volume_id=?1"
    } else {
        ""
    };
    let sql = format!(
        "SELECT f.volume_id, f.path, f.name, f.is_dir, f.ext
         FROM files f
         WHERE f.status='online' AND (
           (f.is_dir=1 AND f.name='.git') OR
           (f.is_dir=0 AND (
              f.name IN ('platformio.ini','Makefile','CMakeLists.txt','package.json')
              OR f.ext IN ('kicad_pro','ioc','sln'))
           )
         ){volume_clause}"
    );
    let mut statement = conn.prepare(&sql)?;
    let mut rows = if volume.is_some() {
        statement.query(params![volume])?
    } else {
        statement.query([])?
    };

    // One entry per (directory, marker kind): a KiCad folder with five
    // .kicad_pcb files still counts as a single KiCad marker.
    let mut grouped: BTreeMap<(String, String), BTreeMap<Marker, usize>> = BTreeMap::new();
    let mut suppressed = HashSet::new();
    while let Some(row) = rows.next()? {
        let volume_id: String = row.get(0)?;
        let path: String = row.get(1)?;
        let name: String = row.get(2)?;
        let is_dir: i64 = row.get(3)?;
        let ext: Option<String> = row.get(4)?;
        let Some(marker) = Marker::detect(&name, ext.as_deref(), is_dir != 0) else {
            continue;
        };
        let Some(parent) = parent_dir(&path) else {
            continue;
        };
        if is_suppressed(parent) {
            suppressed.insert(parent.to_string());
            continue;
        }
        let kind = (volume_id, parent.to_string());
        *grouped.entry(kind).or_default().entry(marker).or_insert(0) += 1;
    }

    let mut candidates = Vec::new();
    for ((volume_id, root_path), markers) in grouped {
        // Rule: at least two distinct marker kinds. A lone package.json or a
        // lone .git is not a project root.
        if markers.len() < 2 {
            continue;
        }
        candidates.push(Candidate {
            depth: depth_of(&root_path),
            volume_id,
            root_path,
            markers,
        });
    }
    // Shallow first, so a nested project (deeper) can overwrite its parent.
    candidates.sort_by(|left, right| {
        left.depth
            .cmp(&right.depth)
            .then_with(|| left.root_path.cmp(&right.root_path))
    });
    Ok((candidates, suppressed.len()))
}

fn assign_subtree(conn: &Connection, volume_id: &str, root_path: &str, id: &str) -> Result<u64> {
    let lower = format!("{root_path}/");
    let upper = format!("{root_path}/\u{10FFFF}");
    let changed = conn.execute(
        "UPDATE files SET project_id=?1
         WHERE volume_id=?2
           AND (path=?3 OR (path>=?4 AND path<?5))
           AND project_id IS NOT ?1",
        params![id, volume_id, root_path, lower, upper],
    )?;
    Ok(changed as u64)
}

fn clear_subtree(conn: &Connection, volume_id: &str, root_path: &str, id: &str) -> Result<u64> {
    let lower = format!("{root_path}/");
    let upper = format!("{root_path}/\u{10FFFF}");
    let changed = conn.execute(
        "UPDATE files SET project_id=NULL
         WHERE volume_id=?1 AND project_id=?2
           AND (path=?3 OR (path>=?4 AND path<?5))",
        params![volume_id, id, root_path, lower, upper],
    )?;
    Ok(changed as u64)
}

fn subtree_stats(conn: &Connection, id: &str) -> Result<(u64, u64)> {
    Ok(conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(size),0)
         FROM files WHERE project_id=?1 AND is_dir=0 AND status='online'",
            params![id],
            |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
        )
        .optional()?
        .unwrap_or_default())
}

/// Rebuild the `projects` table and the `files.project_id` column.
pub fn rebuild_projects(
    conn: &mut Connection,
    mode: RebuildMode,
    volume: Option<&str>,
) -> Result<ProjectReport> {
    ensure_schema(conn)?;
    let started = Instant::now();
    let (candidates, suppressed_marker_dirs) = collect_candidates(conn, volume)?;

    let transaction = conn.transaction()?;
    let mut assigned_rows = 0u64;
    let mut cleared_rows = 0u64;

    if mode == RebuildMode::Full {
        // Scoped to the volume being rebuilt: a `--volume` run must not wipe
        // assignments that belong to other volumes.
        cleared_rows = match volume {
            Some(volume_id) => transaction.execute(
                "UPDATE files SET project_id=NULL WHERE project_id IS NOT NULL AND volume_id=?1",
                params![volume_id],
            )? as u64,
            None => transaction.execute(
                "UPDATE files SET project_id=NULL WHERE project_id IS NOT NULL",
                [],
            )? as u64,
        };
        match volume {
            Some(volume_id) => {
                transaction.execute(
                    "DELETE FROM projects WHERE volume_id=?1",
                    params![volume_id],
                )?;
            }
            None => {
                transaction.execute("DELETE FROM projects", [])?;
            }
        }
    }

    let mut previous: BTreeMap<(String, String), String> = BTreeMap::new();
    {
        let mut statement = transaction.prepare("SELECT volume_id, root_path, id FROM projects")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            previous.insert((row.get(0)?, row.get(1)?), row.get(2)?);
        }
    }

    let mut live: HashSet<(String, String)> = HashSet::new();
    for candidate in &candidates {
        let key = (candidate.volume_id.clone(), candidate.root_path.clone());
        live.insert(key.clone());
        let id = project_id(&candidate.volume_id, &candidate.root_path);
        if mode == RebuildMode::Incremental {
            if let Some(old_id) = previous.get(&key) {
                if old_id != &id {
                    cleared_rows += clear_subtree(
                        &transaction,
                        &candidate.volume_id,
                        &candidate.root_path,
                        old_id,
                    )?;
                }
            }
        }
        assigned_rows += assign_subtree(
            &transaction,
            &candidate.volume_id,
            &candidate.root_path,
            &id,
        )?;
    }

    if mode == RebuildMode::Incremental {
        // Roots that disappeared: release their subtree. A nested project inside
        // a removed root keeps its own assignment because the deeper rebuild
        // happens later in this same loop, but clearing uses the old id only.
        for ((volume_id, root_path), old_id) in &previous {
            if live.contains(&(volume_id.clone(), root_path.clone())) {
                continue;
            }
            cleared_rows += clear_subtree(&transaction, volume_id, root_path, old_id)?;
            transaction.execute(
                "DELETE FROM projects WHERE volume_id=?1 AND root_path=?2",
                params![volume_id, root_path],
            )?;
        }
        // Rows whose project was replaced by a nested root need reassignment:
        // the nested `assign_subtree` runs after the parent because candidates
        // are sorted by depth, so it is already correct.
        cleared_rows += transaction.execute(
            "UPDATE files SET project_id=NULL
             WHERE project_id IS NOT NULL
               AND project_id NOT IN (SELECT id FROM projects)",
            [],
        )? as u64;
    }

    let timestamp = now_iso(&transaction);
    let mut projects = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let id = project_id(&candidate.volume_id, &candidate.root_path);
        let (file_count, bytes) = subtree_stats(&transaction, &id)?;
        let kind = ProjectKind::infer(&candidate.markers);
        let markers: Vec<String> = candidate
            .markers
            .keys()
            .map(|marker| marker.label().to_string())
            .collect();
        transaction.execute(
            "INSERT INTO projects (id, volume_id, root_path, kind, markers, depth, file_count, bytes, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(id) DO UPDATE SET
               kind=excluded.kind, markers=excluded.markers, depth=excluded.depth,
               file_count=excluded.file_count, bytes=excluded.bytes, updated_at=excluded.updated_at",
            params![
                id,
                candidate.volume_id,
                candidate.root_path,
                kind.as_str(),
                serde_json::to_string(&markers).unwrap_or_else(|_| "[]".to_string()),
                candidate.depth as i64,
                file_count as i64,
                bytes as i64,
                timestamp
            ],
        )?;
        projects.push(Project {
            id,
            volume_id: candidate.volume_id,
            root_path: candidate.root_path,
            kind,
            markers,
            depth: candidate.depth,
            file_count,
            bytes,
        });
    }

    transaction.execute(
        "INSERT INTO meta (key, value) VALUES ('projects_built_at', ?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![timestamp],
    )?;
    transaction.commit()?;

    Ok(ProjectReport {
        mode: mode.as_str(),
        roots: projects.len(),
        assigned_rows,
        cleared_rows,
        suppressed_marker_dirs,
        took_ms: started.elapsed().as_millis() as u64,
        projects,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn memory_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(walker_index::SCHEMA_SQL).unwrap();
        conn.execute_batch(
            "INSERT INTO volumes (id,name,mount_path,status,last_seen_at,created_at,updated_at)
             VALUES ('v1','m3','/mnt/m3','online','now','now','now');",
        )
        .unwrap();
        conn
    }

    fn insert(conn: &Connection, path: &str, is_dir: bool, size: i64) {
        let name = path.rsplit('/').next().unwrap();
        let parent = path.rsplit_once('/').map(|(rest, _)| rest).unwrap();
        conn.execute(
            "INSERT INTO files (id,volume_id,parent_id,name,norm_name,path,is_dir,ext,size,status,file_type,created_at,updated_at)
             VALUES (?1,'v1',(SELECT id FROM files WHERE path=?2),?3,?3,?4,?5,?6,?7,'online',?8,'now','now')",
            params![
                path,
                parent,
                name,
                path,
                is_dir as i64,
                walker_core::extension_of(name),
                size,
                if is_dir { "dir" } else { "file" }
            ],
        )
        .unwrap();
    }

    #[test]
    fn detects_two_marker_root_and_inherits() {
        let mut conn = memory_db();
        for (path, is_dir) in [
            ("/mnt/m3/work/serial", true),
            ("/mnt/m3/work/serial/.git", true),
            ("/mnt/m3/work/serial/package.json", false),
            ("/mnt/m3/work/serial/scripts", true),
            ("/mnt/m3/work/serial/scripts/uart.py", false),
        ] {
            insert(&conn, path, is_dir, 10);
        }
        insert(&conn, "/mnt/m3/work/serial/.git/HEAD", false, 5);
        let report = rebuild_projects(&mut conn, RebuildMode::Full, None).unwrap();
        assert_eq!(report.roots, 1);
        assert_eq!(report.projects[0].kind, ProjectKind::Node);
        let inherited: String = conn
            .query_row(
                "SELECT project_id FROM files WHERE path='/mnt/m3/work/serial/scripts/uart.py'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(inherited, report.projects[0].id);
        let outside: Option<String> = conn
            .query_row(
                "SELECT project_id FROM files WHERE path='/mnt/m3/work/serial'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(outside.as_deref(), Some(report.projects[0].id.as_str()));
    }

    #[test]
    fn suppressed_dirs_are_not_roots() {
        let mut conn = memory_db();
        for (path, is_dir) in [
            ("/mnt/m3/work/node_modules/dep", true),
            ("/mnt/m3/work/node_modules/dep/package.json", false),
            ("/mnt/m3/work/node_modules/dep/Makefile", false),
            ("/mnt/m3/work/vendor/lib", true),
            ("/mnt/m3/work/vendor/lib/package.json", false),
            ("/mnt/m3/work/vendor/lib/Makefile", false),
            ("/mnt/m3/work/app", true),
            ("/mnt/m3/work/app/package.json", false),
            ("/mnt/m3/work/app/Makefile", false),
        ] {
            insert(&conn, path, is_dir, 3);
        }
        let report = rebuild_projects(&mut conn, RebuildMode::Full, None).unwrap();
        assert_eq!(report.roots, 1);
        assert_eq!(report.projects[0].root_path, "/mnt/m3/work/app");
        assert!(report.suppressed_marker_dirs >= 2);
    }

    #[test]
    fn nested_monorepo_uses_deepest_root() {
        let mut conn = memory_db();
        for (path, is_dir) in [
            ("/mnt/m3/repo", true),
            ("/mnt/m3/repo/.git", true),
            ("/mnt/m3/repo/Makefile", false),
            ("/mnt/m3/repo/packages/lib", true),
            ("/mnt/m3/repo/packages/lib/package.json", false),
            ("/mnt/m3/repo/packages/lib/.git", true),
            ("/mnt/m3/repo/packages/lib/src/a.ts", false),
        ] {
            insert(&conn, path, is_dir, 4);
        }
        insert(&conn, "/mnt/m3/repo/.git/config", false, 1);
        insert(&conn, "/mnt/m3/repo/packages/lib/.git/config", false, 1);
        let report = rebuild_projects(&mut conn, RebuildMode::Full, None).unwrap();
        assert_eq!(report.roots, 2);
        let outer = report
            .projects
            .iter()
            .find(|p| p.root_path == "/mnt/m3/repo")
            .unwrap();
        let inner = report
            .projects
            .iter()
            .find(|p| p.root_path == "/mnt/m3/repo/packages/lib")
            .unwrap();
        let inner_owner: String = conn
            .query_row(
                "SELECT project_id FROM files WHERE path='/mnt/m3/repo/packages/lib/src/a.ts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(inner_owner, inner.id);
        let outer_owner: String = conn
            .query_row(
                "SELECT project_id FROM files WHERE path='/mnt/m3/repo/Makefile'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(outer_owner, outer.id);
    }

    #[test]
    fn kicad_folder_counts_marker_kinds_once() {
        let mut conn = memory_db();
        for (path, is_dir) in [
            ("/mnt/m3/board", true),
            ("/mnt/m3/board/board.kicad_pro", false),
            ("/mnt/m3/board/a.kicad_pcb", false),
            ("/mnt/m3/board/b.kicad_pcb", false),
            ("/mnt/m3/board/c.kicad_sch", false),
            ("/mnt/m3/board/Makefile", false),
            ("/mnt/m3/board/board.ioc", false),
            ("/mnt/m3/board/src/main.c", false),
        ] {
            insert(&conn, path, is_dir, 7);
        }
        let report = rebuild_projects(&mut conn, RebuildMode::Full, None).unwrap();
        assert_eq!(report.roots, 1);
        let project = &report.projects[0];
        assert_eq!(project.kind, ProjectKind::Kicad);
        assert_eq!(project.markers.len(), 3);
        assert!(project.markers.iter().any(|m| m == "*.kicad_pro"));
    }

    #[test]
    fn incremental_add_and_remove_roots() {
        let mut conn = memory_db();
        for (path, is_dir) in [
            ("/mnt/m3/a", true),
            ("/mnt/m3/a/.git", true),
            ("/mnt/m3/a/Makefile", false),
            ("/mnt/m3/a/x.c", false),
        ] {
            insert(&conn, path, is_dir, 2);
        }
        insert(&conn, "/mnt/m3/a/.git/config", false, 1);
        let first = rebuild_projects(&mut conn, RebuildMode::Full, None).unwrap();
        assert_eq!(first.roots, 1);

        insert(&conn, "/mnt/m3/b", true, 0);
        insert(&conn, "/mnt/m3/b/package.json", false, 2);
        insert(&conn, "/mnt/m3/b/CMakeLists.txt", false, 2);
        let second = rebuild_projects(&mut conn, RebuildMode::Incremental, None).unwrap();
        assert_eq!(second.roots, 2);

        conn.execute(
            "UPDATE files SET status='missing' WHERE path='/mnt/m3/b/package.json'",
            [],
        )
        .unwrap();
        let third = rebuild_projects(&mut conn, RebuildMode::Incremental, None).unwrap();
        assert_eq!(third.roots, 1);
        let orphan: Option<String> = conn
            .query_row(
                "SELECT project_id FROM files WHERE path='/mnt/m3/b/CMakeLists.txt'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(orphan.is_none());
    }
}
