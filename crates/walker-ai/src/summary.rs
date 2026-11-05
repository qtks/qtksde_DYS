//! Directory summaries.
//!
//! Default is a *local extractive* summary: deterministic statistics over the
//! M1 index (counts, dominant extensions, project kinds, largest/ newest
//! files) plus a one-sentence digest. No model is loaded, no file content
//! leaves the machine, and no cloud call happens.
//!
//! A cloud path exists but is off by default: `--cloud` requires `--consent`,
//! and the CLI must print the exact upload scope (paths, sizes, characters)
//! before the request is built.
//!
//! A generative local model is not shipped: bge-small-zh-v1.5 is a retrieval
//! encoder, not a summarizer, and bundling a decoder would add hundreds of
//! megabytes without changing the acceptance target. The extractive fallback
//! is labelled as such in every response.

use rusqlite::{params, Connection};
use serde::Serialize;

use crate::db::config_error;
use crate::Result;

#[derive(Debug, Clone, Serialize)]
pub struct ExtensionBucket {
    pub ext: String,
    pub count: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TopFile {
    pub path: String,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DirSummary {
    pub path: String,
    pub generator: &'static str,
    pub files: u64,
    pub subdirs: u64,
    pub bytes: u64,
    /// Files whose name looks textual (extension-based estimate, used only for
    /// reporting; embedding classification never trusts extensions).
    pub text_like_files: u64,
    pub project_count: u64,
    pub projects: Vec<String>,
    pub top_extensions: Vec<ExtensionBucket>,
    pub largest_files: Vec<TopFile>,
    pub newest_file: Option<TopFile>,
    pub digest: String,
    pub note: String,
}

fn ext_of(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => ext.to_lowercase(),
        _ => "(none)".to_string(),
    }
}

pub fn summarize_dir(conn: &Connection, path: &str, limit: usize) -> Result<DirSummary> {
    let prefix = format!("{}/", path.trim_end_matches('/'));
    let upper = format!("{}/{}", path.trim_end_matches('/'), '\u{10FFFF}');

    let (files, subdirs, bytes, text_like_files): (u64, u64, u64, u64) = conn.query_row(
        "SELECT
            COALESCE(SUM(CASE WHEN is_dir=0 THEN 1 ELSE 0 END),0),
            COALESCE(SUM(CASE WHEN is_dir=1 THEN 1 ELSE 0 END),0),
            COALESCE(SUM(CASE WHEN is_dir=0 THEN size ELSE 0 END),0),
            COALESCE(SUM(CASE WHEN is_dir=0 AND (lower(name) LIKE '%.txt' OR lower(name) LIKE '%.md'
                 OR lower(name) LIKE '%.py' OR lower(name) LIKE '%.rs' OR lower(name) LIKE '%.c'
                 OR lower(name) LIKE '%.js' OR lower(name) LIKE '%.json' OR lower(name) LIKE '%.csv') THEN 1 ELSE 0 END),0)
         FROM files
         WHERE status='online' AND ((path>=?1 AND path<?2) OR path=?3)",
        params![prefix, upper, path.trim_end_matches('/')],
        |row| {
            Ok((
                row.get::<_, i64>(0)? as u64,
                row.get::<_, i64>(1)? as u64,
                row.get::<_, i64>(2)? as u64,
                row.get::<_, i64>(3)? as u64,
            ))
        },
    )?;

    let mut extension_buckets: Vec<(String, u64, u64)>;
    {
        let mut statement = conn.prepare(
            "SELECT name, size FROM files
             WHERE status='online' AND is_dir=0 AND ((path>=?1 AND path<?2) OR path=?3)
             LIMIT 5000",
        )?;
        let mut rows = statement.query(params![prefix, upper, path.trim_end_matches('/')])?;
        let mut buckets: std::collections::HashMap<String, (u64, u64)> =
            std::collections::HashMap::new();
        while let Some(row) = rows.next()? {
            let name: String = row.get(0)?;
            let size: i64 = row.get(1)?;
            let entry = buckets.entry(ext_of(&name)).or_insert((0, 0));
            entry.0 += 1;
            entry.1 += size.max(0) as u64;
        }
        extension_buckets = buckets
            .into_iter()
            .map(|(ext, (count, bytes))| (ext, count, bytes))
            .collect();
        extension_buckets.sort_by_key(|left| std::cmp::Reverse(left.2));
    }

    let mut largest: Vec<TopFile> = Vec::new();
    {
        let mut statement = conn.prepare(
            "SELECT path, size, mtime FROM files
             WHERE status='online' AND is_dir=0 AND ((path>=?1 AND path<?2) OR path=?3)
             ORDER BY size DESC LIMIT ?4",
        )?;
        let mut rows = statement.query(params![
            prefix,
            upper,
            path.trim_end_matches('/'),
            limit as i64
        ])?;
        while let Some(row) = rows.next()? {
            let size: i64 = row.get::<_, i64>(1)?;
            largest.push(TopFile {
                path: row.get(0)?,
                size: size.max(0) as u64,
                mtime: row.get::<_, i64>(2)?,
            });
        }
    }

    let newest_file: Option<TopFile> = conn
        .query_row(
            "SELECT path, size, mtime FROM files
             WHERE status='online' AND is_dir=0 AND ((path>=?1 AND path<?2) OR path=?3)
             ORDER BY mtime DESC LIMIT 1",
            params![prefix, upper, path.trim_end_matches('/')],
            |row| {
                Ok(TopFile {
                    path: row.get(0)?,
                    size: row.get::<_, i64>(1)?.max(0) as u64,
                    mtime: row.get(2)?,
                })
            },
        )
        .ok();

    let mut projects: Vec<String> = Vec::new();
    let projects_table: bool = conn
        .query_row(
            "SELECT COUNT(*)>0 FROM sqlite_master WHERE type='table' AND name='projects'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);
    if projects_table {
        let mut statement = conn.prepare(
            "SELECT DISTINCT root_path FROM projects
             WHERE root_path>=?1 AND root_path<?2 LIMIT 20",
        )?;
        let mut rows = statement.query(params![prefix, upper])?;
        while let Some(row) = rows.next()? {
            projects.push(row.get::<_, String>(0)?);
        }
    }

    let top_extensions: Vec<ExtensionBucket> = extension_buckets
        .iter()
        .take(6)
        .map(|(ext, count, bytes)| ExtensionBucket {
            ext: ext.clone(),
            count: *count,
            bytes: *bytes,
        })
        .collect();

    let dominant = top_extensions.first();
    let digest = match dominant {
        Some(bucket) if files > 0 => format!(
            "{} 下共 {} 个文件、{} 个子目录，合计 {}；以 .{} 为主（{} 个，占 {:.0}%）。",
            path,
            files,
            subdirs,
            human_bytes(bytes),
            bucket.ext,
            bucket.count,
            bucket.count as f64 / files.max(1) as f64 * 100.0
        ),
        _ => format!(
            "{} 下共 {} 个文件、{} 个子目录，合计 {}。",
            path,
            files,
            subdirs,
            human_bytes(bytes)
        ),
    };

    Ok(DirSummary {
        path: path.to_string(),
        generator: "local-extractive",
        files,
        subdirs,
        bytes,
        text_like_files,
        project_count: projects.len() as u64,
        projects,
        top_extensions,
        largest_files: largest,
        newest_file,
        digest,
        note: "本地抽取式摘要：只统计索引元数据，不调用模型、不上传任何文件内容。".to_string(),
    })
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Exactly what a cloud call would send. Printed *before* any request.
#[derive(Debug, Clone, Serialize)]
pub struct CloudScope {
    pub path: String,
    pub files: u64,
    pub body_bytes: u64,
    pub body_chars: u64,
    pub sample_paths: Vec<String>,
    pub payload_preview: String,
}

pub fn cloud_scope(conn: &Connection, path: &str, max_files: usize) -> Result<CloudScope> {
    let prefix = format!("{}/", path.trim_end_matches('/'));
    let upper = format!("{}/{}", path.trim_end_matches('/'), '\u{10FFFF}');
    let mut statement = conn.prepare(
        "SELECT path, size FROM files
         WHERE status='online' AND is_dir=0 AND size<=524288
           AND ((path>=?1 AND path<?2) OR path=?3)
         ORDER BY size DESC LIMIT ?4",
    )?;
    let mut rows = statement.query(params![
        prefix,
        upper,
        path.trim_end_matches('/'),
        max_files as i64
    ])?;
    let mut paths = Vec::new();
    let mut bytes = 0u64;
    while let Some(row) = rows.next()? {
        paths.push(row.get::<_, String>(0)?);
        bytes += row.get::<_, i64>(1)?.max(0) as u64;
    }
    let mut payload = String::new();
    for item in &paths {
        payload.push_str(item);
        payload.push('\n');
    }
    Ok(CloudScope {
        path: path.to_string(),
        files: paths.len() as u64,
        body_bytes: bytes,
        body_chars: payload.chars().count() as u64,
        sample_paths: paths,
        payload_preview: payload,
    })
}

#[derive(Debug, Clone)]
pub struct CloudConfig {
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
}

pub fn cloud_summarize(scope: &CloudScope, config: &CloudConfig, consent: bool) -> Result<String> {
    if !consent {
        return Err(config_error(
            "cloud summary is off by default; re-run with --consent after reviewing the scope",
        ));
    }
    if config.endpoint.trim().is_empty() {
        return Err(config_error("--endpoint is required for cloud summary"));
    }
    let body = serde_json::json!({
        "model": config.model,
        "messages": [
            {"role": "system", "content": "Summarize the directory in two Chinese sentences."},
            {"role": "user", "content": format!("目录：{}\n文件清单：\n{}", scope.path, scope.payload_preview)}
        ]
    });
    let response = ureq::post(&config.endpoint)
        .header("Authorization", format!("Bearer {}", config.api_key))
        .header("Content-Type", "application/json")
        .send(
            serde_json::to_string(&body)
                .map_err(|error| config_error(format!("cloud summary payload: {error}")))?,
        )
        .map_err(|error| config_error(format!("cloud summary request failed: {error}")))?;
    let text = response
        .into_body()
        .read_to_string()
        .map_err(|error| config_error(format!("cloud summary response failed: {error}")))?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_detection_ignores_dotfiles() {
        assert_eq!(ext_of("main.rs"), "rs");
        assert_eq!(ext_of(".gitignore"), "(none)");
        assert_eq!(ext_of("Makefile"), "(none)");
    }
}
