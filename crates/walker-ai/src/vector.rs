//! Sharded, normalized float32 vector store on top of the index database.
//!
//! Vectors are partitioned by volume + top-level directory. Each shard is
//! scanned in memory with a brute-force dot product (vectors are L2-normalized,
//! so dot product == cosine similarity). At 100k vectors this stays in the low
//! tens of milliseconds thanks to rayon; see `bench_vectors`.
//!
//! Only three tables are written: `vectors`, `vector_shards`, and `meta`.
//! User files are never opened for writing.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use rayon::prelude::*;
use rusqlite::{named_params, params, Connection};
use serde::Serialize;

use crate::db::{ensure_schema, now_iso};
use crate::embed::{self, ContentKind, Embedder, EMBED_DIM};
use crate::Result;

#[derive(Debug, Clone, Serialize)]
pub struct VectorRecord {
    pub file_id: String,
    pub shard: String,
    pub dim: usize,
    pub vec: Vec<f32>,
    pub content_hash: String,
    pub mtime: i64,
    pub size: i64,
    pub model: String,
}

/// A shard key is `<volume_id>/<first directory under the mount>`.
/// Files that sit directly under the mount share the `(root)` shard, so a
/// flat directory does not produce one shard per file.
pub fn shard_for(volume_id: &str, mount_path: &str, path: &str) -> String {
    let mount = mount_path.trim_end_matches(['/', '\\']);
    let rest = path
        .strip_prefix(mount)
        .unwrap_or(path)
        .trim_start_matches(['/', '\\']);
    let components: Vec<&str> = rest
        .split(['/', '\\'])
        .filter(|component| !component.is_empty())
        .collect();
    match components.len() {
        0 | 1 => format!("{volume_id}/(root)"),
        _ => format!("{volume_id}/{}", components[0]),
    }
}

pub fn encode_vec(values: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(values.len() * 4);
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

pub fn decode_vec(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

pub fn write_vector(conn: &Connection, record: &VectorRecord, updated_at: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO vectors (file_id, shard, dim, vec, content_hash, mtime, size, model, updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
         ON CONFLICT(file_id) DO UPDATE SET
           shard=excluded.shard, dim=excluded.dim, vec=excluded.vec,
           content_hash=excluded.content_hash, mtime=excluded.mtime,
           size=excluded.size, model=excluded.model, updated_at=excluded.updated_at",
        params![
            record.file_id,
            record.shard,
            record.dim as i64,
            encode_vec(&record.vec),
            record.content_hash,
            record.mtime,
            record.size,
            record.model,
            updated_at
        ],
    )?;
    Ok(())
}

pub fn refresh_shard_stats(conn: &Connection, shards: &HashSet<String>) -> Result<()> {
    let timestamp = now_iso(conn);
    for shard in shards {
        let rows: i64 = conn.query_row(
            "SELECT COUNT(*) FROM vectors WHERE shard=?1",
            params![shard],
            |row| row.get(0),
        )?;
        let (volume_id, top_dir) = match shard.split_once('/') {
            Some((volume, rest)) => (volume.to_string(), rest.to_string()),
            None => (String::new(), shard.clone()),
        };
        conn.execute(
            "INSERT INTO vector_shards (shard, volume_id, top_dir, rows, dim, model, built_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(shard) DO UPDATE SET rows=excluded.rows, volume_id=excluded.volume_id,
               top_dir=excluded.top_dir, dim=excluded.dim, model=excluded.model,
               built_at=excluded.built_at",
            params![
                shard,
                volume_id,
                top_dir,
                rows,
                EMBED_DIM as i64,
                embed::MODEL_NAME,
                timestamp
            ],
        )?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct ScoredVector {
    pub file_id: String,
    pub shard: String,
    pub score: f32,
}

/// Brute-force scan of one shard, top-k by cosine. Vectors are normalized at
/// write time, so a dot product is the cosine.
pub fn search_shard(rows: &[(String, Vec<f32>)], query: &[f32], k: usize) -> Vec<ScoredVector> {
    let mut scored: Vec<ScoredVector> = rows
        .par_iter()
        .map(|(file_id, vector)| {
            let dot = if vector.len() == query.len() {
                vector
                    .iter()
                    .zip(query)
                    .map(|(left, right)| left * right)
                    .sum::<f32>()
            } else {
                f32::NEG_INFINITY
            };
            ScoredVector {
                file_id: file_id.clone(),
                shard: String::new(),
                score: dot,
            }
        })
        .collect();
    scored.sort_by(|left, right| right.score.total_cmp(&left.score));
    scored.truncate(k);
    scored
}

pub fn load_shard(conn: &Connection, shard: &str) -> Result<Vec<(String, Vec<f32>)>> {
    let mut statement =
        conn.prepare("SELECT file_id, vec FROM vectors WHERE shard=?1 AND dim=?2")?;
    let mut rows = statement.query(params![shard, EMBED_DIM as i64])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let file_id: String = row.get(0)?;
        let bytes: Vec<u8> = row.get(1)?;
        out.push((file_id, decode_vec(&bytes)));
    }
    Ok(out)
}

pub fn list_shards(conn: &Connection) -> Result<Vec<String>> {
    let mut statement = conn.prepare("SELECT DISTINCT shard FROM vectors ORDER BY shard")?;
    let mut rows = statement.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(row.get::<_, String>(0)?);
    }
    Ok(out)
}

#[derive(Debug, Clone, Default)]
pub struct EmbedOptions {
    pub limit: Option<usize>,
    pub batch: usize,
    pub volume: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EmbedReport {
    pub model: &'static str,
    pub dim: usize,
    pub candidates: u64,
    pub embedded: u64,
    pub skipped_unchanged: u64,
    pub skipped_content_same: u64,
    pub skipped_binary: u64,
    pub skipped_unreadable: u64,
    pub errors: u64,
    pub vectors_total: u64,
    pub shards: u64,
    pub batches: u64,
    pub files_per_second: f64,
    pub took_ms: u64,
    pub estimated_100k_seconds: f64,
    pub first_errors: Vec<String>,
}

struct CandidateRow {
    file_id: String,
    shard: String,
    path: String,
    name: String,
    size: u64,
    mtime: i64,
}

/// Embed every text file that is missing a vector or whose `(mtime, size)`
/// changed. A changed file is re-hashed before re-running the model, so a
/// touched-but-identical file costs a 4 KiB read instead of an inference.
pub fn embed_corpus(
    conn: &mut Connection,
    embedder: &mut Embedder,
    options: &EmbedOptions,
) -> Result<EmbedReport> {
    ensure_schema(conn)?;
    let started = Instant::now();
    let batch_size = options.batch.clamp(1, 64);
    let model = embedder.info.name;

    let mut existing: HashMap<String, (String, i64, i64)> = HashMap::new();
    {
        let mut statement =
            conn.prepare("SELECT file_id, content_hash, mtime, size FROM vectors")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            existing.insert(
                row.get::<_, String>(0)?,
                (
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ),
            );
        }
    }

    let sql = "SELECT f.id, f.volume_id, v.mount_path, f.path, f.name, f.size, f.mtime
               FROM files f JOIN volumes v ON v.id=f.volume_id AND v.status='online'
               WHERE f.is_dir=0 AND f.status='online' AND f.size<=:max_size
                 AND (:volume IS NULL OR f.volume_id=:volume)
               LIMIT COALESCE(:limit, -1)";
    let mut candidates: Vec<CandidateRow> = Vec::new();
    {
        let max_size = embed::MAX_EMBED_FILE_BYTES as i64;
        let mut statement = conn.prepare(sql)?;
        let mut rows = statement.query(named_params! {
            ":max_size": max_size,
            ":volume": options.volume.as_deref(),
            ":limit": options.limit.map(|limit| limit as i64),
        })?;
        while let Some(row) = rows.next()? {
            let volume_id: String = row.get(1)?;
            let mount: String = row.get(2)?;
            let path: String = row.get(3)?;
            candidates.push(CandidateRow {
                file_id: row.get(0)?,
                shard: shard_for(&volume_id, &mount, &path),
                path,
                name: row.get(4)?,
                size: row.get::<_, i64>(5)?.max(0) as u64,
                mtime: row.get(6)?,
            });
        }
    }

    let mut report = EmbedReport {
        model,
        dim: EMBED_DIM,
        candidates: candidates.len() as u64,
        embedded: 0,
        skipped_unchanged: 0,
        skipped_content_same: 0,
        skipped_binary: 0,
        skipped_unreadable: 0,
        errors: 0,
        vectors_total: 0,
        shards: 0,
        batches: 0,
        files_per_second: 0.0,
        took_ms: 0,
        estimated_100k_seconds: 0.0,
        first_errors: Vec::new(),
    };

    let mut touched_shards: HashSet<String> = HashSet::new();
    let mut batch: Vec<embed::TextSample> = Vec::new();

    for candidate in &candidates {
        if let Some((hash, mtime, size)) = existing.get(&candidate.file_id) {
            if *mtime == candidate.mtime && *size == candidate.size as i64 && !hash.is_empty() {
                report.skipped_unchanged += 1;
                continue;
            }
        }
        let (kind, sample) = embed::sample_for(
            &candidate.file_id,
            &candidate.shard,
            &candidate.path,
            &candidate.name,
            candidate.size,
            candidate.mtime,
        );
        let Some(sample) = sample else {
            match kind {
                ContentKind::Unreadable => report.skipped_unreadable += 1,
                _ => report.skipped_binary += 1,
            }
            continue;
        };
        let hash = blake3::hash(sample.text.as_bytes()).to_hex().to_string();
        if let Some((old_hash, _, _)) = existing.get(&candidate.file_id) {
            if old_hash == &hash {
                report.skipped_content_same += 1;
                continue;
            }
        }
        batch.push(sample);
        if batch.len() >= batch_size {
            flush_batch(conn, embedder, &mut batch, &mut report, &mut touched_shards)?;
            report.batches += 1;
        }
    }
    if !batch.is_empty() {
        flush_batch(conn, embedder, &mut batch, &mut report, &mut touched_shards)?;
        report.batches += 1;
    }

    refresh_shard_stats(conn, &touched_shards)?;

    report.vectors_total = conn.query_row("SELECT COUNT(*) FROM vectors", [], |row| {
        row.get::<_, i64>(0)
    })? as u64;
    report.shards = conn.query_row("SELECT COUNT(DISTINCT shard) FROM vectors", [], |row| {
        row.get::<_, i64>(0)
    })? as u64;
    report.took_ms = started.elapsed().as_millis() as u64;
    let seconds = (report.took_ms as f64 / 1000.0).max(0.001);
    report.files_per_second = report.embedded as f64 / seconds;
    if report.files_per_second > 0.0 {
        report.estimated_100k_seconds = 100_000.0 / report.files_per_second;
    }
    Ok(report)
}

fn flush_batch(
    conn: &mut Connection,
    embedder: &mut Embedder,
    batch: &mut Vec<embed::TextSample>,
    report: &mut EmbedReport,
    touched_shards: &mut HashSet<String>,
) -> Result<()> {
    let texts: Vec<String> = batch.iter().map(|sample| sample.text.clone()).collect();
    let vectors = match embedder.embed(&texts) {
        Ok(vectors) => vectors,
        Err(error) => {
            report.errors += batch.len() as u64;
            if report.first_errors.len() < 5 {
                report.first_errors.push(error.to_string());
            }
            batch.clear();
            return Ok(());
        }
    };
    let timestamp = now_iso(conn);
    let transaction = conn.transaction()?;
    for (sample, vector) in batch.iter().zip(vectors) {
        let record = VectorRecord {
            file_id: sample.file_id.clone(),
            shard: sample.shard.clone(),
            dim: vector.len(),
            vec: vector,
            content_hash: blake3::hash(sample.text.as_bytes()).to_hex().to_string(),
            mtime: sample.mtime,
            size: sample.size,
            model: embed::MODEL_NAME.to_string(),
        };
        write_vector(&transaction, &record, &timestamp)?;
        touched_shards.insert(record.shard.clone());
        report.embedded += 1;
    }
    transaction.commit()?;
    batch.clear();
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct VectorBenchReport {
    pub rows: usize,
    pub dim: usize,
    pub shards: usize,
    pub queries: usize,
    pub k: usize,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub worst_ms: f64,
    pub avg_ms: f64,
    pub build_ms: u64,
}

/// Synthetic benchmark: N random normalized vectors across S shards, Q queries.
/// This measures the exact scan path used by `search_shard`, without SQLite.
pub fn bench_vectors(
    rows: usize,
    dim: usize,
    shards: usize,
    queries: usize,
    k: usize,
) -> VectorBenchReport {
    let started = Instant::now();
    let shards = shards.max(1);
    let mut state = 0x2545F4914F6CDD1Du64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state as f32 / u64::MAX as f32) - 0.5
    };
    let mut banks: Vec<Vec<(String, Vec<f32>)>> = (0..shards).map(|_| Vec::new()).collect();
    for index in 0..rows {
        let mut vector: Vec<f32> = (0..dim).map(|_| next()).collect();
        let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
        for value in &mut vector {
            *value /= norm.max(f32::EPSILON);
        }
        banks[index % shards].push((format!("f{index:08}"), vector));
    }
    let build_ms = started.elapsed().as_millis() as u64;

    let mut timings = Vec::with_capacity(queries);
    for _ in 0..queries {
        let mut query: Vec<f32> = (0..dim).map(|_| next()).collect();
        let norm = query.iter().map(|value| value * value).sum::<f32>().sqrt();
        for value in &mut query {
            *value /= norm.max(f32::EPSILON);
        }
        let scan = Instant::now();
        let mut merged: Vec<ScoredVector> = Vec::new();
        for bank in &banks {
            merged.extend(search_shard(bank, &query, k));
        }
        merged.sort_by(|left, right| right.score.total_cmp(&left.score));
        merged.truncate(k);
        timings.push(scan.elapsed().as_secs_f64() * 1000.0);
    }
    timings.sort_by(|left, right| left.total_cmp(right));
    let total: f64 = timings.iter().sum();
    VectorBenchReport {
        rows,
        dim,
        shards,
        queries,
        k,
        p50_ms: timings.get(timings.len() / 2).copied().unwrap_or(0.0),
        p95_ms: timings
            .get((timings.len() * 95 / 100).min(timings.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0.0),
        worst_ms: timings.last().copied().unwrap_or(0.0),
        avg_ms: total / timings.len().max(1) as f64,
        build_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shard_key_uses_top_level_dir() {
        assert_eq!(
            shard_for("v1", "/mnt/v1", "/mnt/v1/projects/a/x.c"),
            "v1/projects"
        );
        assert_eq!(shard_for("v1", "/mnt/v1", "/mnt/v1"), "v1/(root)");
        assert_eq!(shard_for("v1", "/mnt/v1", "/mnt/v1/x.c"), "v1/(root)");
    }

    #[test]
    fn vector_roundtrip_is_lossless() {
        let values = vec![0.1f32, -0.25, 0.5];
        let bytes = encode_vec(&values);
        assert_eq!(decode_vec(&bytes), values);
    }

    #[test]
    fn brute_force_scan_orders_by_similarity() {
        let query = vec![1.0f32, 0.0];
        let rows = vec![
            ("a".to_string(), vec![1.0f32, 0.0]),
            ("b".to_string(), vec![0.0f32, 1.0]),
            ("c".to_string(), vec![-1.0f32, 0.0]),
        ];
        let hits = search_shard(&rows, &query, 2);
        assert_eq!(hits[0].file_id, "a");
        assert_eq!(hits[1].file_id, "b");
    }
}
