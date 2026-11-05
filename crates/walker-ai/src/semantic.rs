//! Semantic search: embed the query, scan every shard, merge, optionally
//! blend with the M2 FTS index.
//!
//! Score normalization is explicit so results stay comparable across shards:
//! * semantic: `(cosine + 1) / 2` maps `[-1, 1]` onto `[0, 1]`.
//! * FTS: position in the (unranked) trigram result list, `1 / (1 + position)`.
//!   Ranking bm25 over a 1M-row trigram index costs seconds, so hybrid mode
//!   deliberately uses the cheap positional score.
//! * blend: `0.65 * semantic + 0.35 * fts`; a document present in only one
//!   list gets its own weighted term and a zero for the missing one.

use std::collections::HashMap;
use std::time::Instant;

use rusqlite::{params, Connection};
use serde::Serialize;

use crate::embed::Embedder;
use crate::vector;
use crate::Result;

pub const SEMANTIC_WEIGHT: f32 = 0.65;
pub const FTS_WEIGHT: f32 = 0.35;

#[derive(Debug, Clone, Serialize)]
pub struct SemanticHit {
    pub file_id: String,
    pub path: String,
    pub name: String,
    pub size: u64,
    pub shard: String,
    pub score: f32,
    pub semantic: f32,
    pub fts: f32,
    pub source: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct SemanticReport {
    pub query: String,
    pub k: usize,
    pub hybrid: bool,
    pub shards: usize,
    pub scanned_vectors: u64,
    pub embed_ms: f64,
    pub scan_ms: f64,
    pub took_ms: u64,
    pub hits: Vec<SemanticHit>,
    pub note: String,
}

pub struct SemanticOptions {
    pub k: usize,
    pub hybrid: bool,
    pub shard: Option<String>,
}

impl Default for SemanticOptions {
    fn default() -> Self {
        Self {
            k: 10,
            hybrid: false,
            shard: None,
        }
    }
}

pub fn search(
    conn: &Connection,
    embedder: &mut Embedder,
    query: &str,
    options: &SemanticOptions,
) -> Result<SemanticReport> {
    let started = Instant::now();
    let vectors_table: bool = conn.query_row(
        "SELECT COUNT(*)>0 FROM sqlite_master WHERE type='table' AND name='vectors'",
        [],
        |row| row.get(0),
    )?;
    if !vectors_table {
        return Err(crate::db::config_error(
            "no vectors table in this index; run `walker-ai embed` first",
        ));
    }
    let embed_started = Instant::now();
    let vector = embedder.embed(&[query.to_string()])?.into_iter().next();
    let query_vector = match vector {
        Some(vector) => vector,
        None => return Err(crate::db::config_error("query embedding was empty")),
    };
    let embed_ms = embed_started.elapsed().as_secs_f64() * 1000.0;

    let scan_started = Instant::now();
    let shards: Vec<String> = match options.shard.as_deref() {
        Some(shard) => vec![shard.to_string()],
        None => vector::list_shards(conn)?,
    };
    let mut scanned_vectors = 0u64;
    let mut best: HashMap<String, (f32, String)> = HashMap::new();
    for shard in &shards {
        let rows = vector::load_shard(conn, shard)?;
        scanned_vectors += rows.len() as u64;
        for mut hit in vector::search_shard(&rows, &query_vector, options.k) {
            if !hit.score.is_finite() {
                continue;
            }
            hit.shard = shard.clone();
            best.entry(hit.file_id.clone())
                .and_modify(|entry| {
                    if hit.score > entry.0 {
                        *entry = (hit.score, shard.clone());
                    }
                })
                .or_insert((hit.score, shard.clone()));
        }
    }
    let scan_ms = scan_started.elapsed().as_secs_f64() * 1000.0;

    let mut semantic: Vec<(String, f32, String)> = best
        .into_iter()
        .map(|(file_id, (score, shard))| {
            let normalized = ((score + 1.0) / 2.0).clamp(0.0, 1.0);
            (file_id, normalized, shard)
        })
        .collect();
    semantic.sort_by(|left, right| right.1.total_cmp(&left.1));
    semantic.truncate(options.k);

    let fts_scores = if options.hybrid {
        fts_positions(conn, query, options.k.saturating_mul(4))?
    } else {
        HashMap::new()
    };

    let mut hits = Vec::with_capacity(semantic.len());
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (file_id, semantic_score, shard) in &semantic {
        let fts = fts_scores.get(file_id).copied().unwrap_or(0.0);
        let score = if options.hybrid {
            SEMANTIC_WEIGHT * semantic_score + FTS_WEIGHT * fts
        } else {
            *semantic_score
        };
        seen.insert(file_id.clone(), hits.len());
        hits.push(SemanticHit {
            file_id: file_id.clone(),
            path: String::new(),
            name: String::new(),
            size: 0,
            shard: shard.clone(),
            score,
            semantic: *semantic_score,
            fts,
            source: "semantic",
        });
    }
    if options.hybrid {
        for (file_id, fts) in &fts_scores {
            if seen.contains_key(file_id) {
                continue;
            }
            hits.push(SemanticHit {
                file_id: file_id.clone(),
                path: String::new(),
                name: String::new(),
                size: 0,
                shard: String::new(),
                score: FTS_WEIGHT * fts,
                semantic: 0.0,
                fts: *fts,
                source: "fts",
            });
        }
    }

    // Join row metadata in one statement rather than per hit.
    {
        let mut statement =
            conn.prepare("SELECT f.path, f.name, f.size, f.status FROM files f WHERE f.id=?1")?;
        for hit in &mut hits {
            if let Ok(row) = statement.query_row(params![hit.file_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            }) {
                hit.path = row.0;
                hit.name = row.1;
                hit.size = row.2.max(0) as u64;
                if row.3 != "online" {
                    hit.score = 0.0;
                }
            }
        }
    }

    hits.sort_by(|left, right| right.score.total_cmp(&left.score));
    hits.retain(|hit| hit.score > 0.0);
    hits.truncate(options.k);
    let note = if options.hybrid {
        format!("hybrid: {SEMANTIC_WEIGHT}*semantic + {FTS_WEIGHT}*fts(positional)")
    } else {
        "semantic only: (cosine+1)/2".to_string()
    };

    Ok(SemanticReport {
        query: query.to_string(),
        k: options.k,
        hybrid: options.hybrid,
        shards: shards.len(),
        scanned_vectors,
        embed_ms,
        scan_ms,
        took_ms: started.elapsed().as_millis() as u64,
        hits,
        note,
    })
}

/// Positional FTS scores over the trigram index: `1/(1+position)`.
/// Only used by hybrid ranking; the plain M2 FTS endpoint keeps its own path.
fn fts_positions(conn: &Connection, query: &str, limit: usize) -> Result<HashMap<String, f32>> {
    // Trigram needs >= 3 characters; shorter queries would match nothing.
    if query.chars().count() < 3 {
        return Ok(HashMap::new());
    }
    // FTS5 treats a bare query as syntax; quote it so user punctuation cannot
    // turn into an "unterminated string" error.
    let quoted = format!("\"{}\"", query.replace('"', "\"\""));
    let mut out = HashMap::new();
    let exists: bool = conn
        .query_row(
            "SELECT COUNT(*)>0 FROM sqlite_master WHERE type='table' AND name='files_fts'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);
    if !exists {
        return Ok(out);
    }
    let mut statement = conn.prepare(
        "SELECT f.id FROM files_fts JOIN files f ON f.rowid=files_fts.rowid
         WHERE files_fts MATCH ?1 AND f.status='online' LIMIT ?2",
    )?;
    let mut rows = statement.query(params![quoted, limit as i64])?;
    let mut position = 0usize;
    while let Some(row) = rows.next()? {
        let file_id: String = row.get(0)?;
        out.insert(file_id, 1.0 / (1.0 + position as f32));
        position += 1;
    }
    Ok(out)
}
