use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Instant;

use rusqlite::{params, Connection};
use serde::Serialize;

use crate::Result;

#[derive(Debug, Clone)]
pub struct DuplicateOptions {
    /// Bytes read from the head and from the tail during the partial-hash stage.
    /// 4 KiB each: one page, enough to separate files that share a size but
    /// differ in content, without paying for the full read of large media files.
    pub partial_bytes: usize,
    pub max_groups: usize,
    pub volume_id: Option<String>,
    pub include_empty: bool,
}

impl Default for DuplicateOptions {
    fn default() -> Self {
        Self {
            partial_bytes: 4096,
            max_groups: 50,
            volume_id: None,
            include_empty: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DuplicateFile {
    pub id: String,
    pub path: String,
    pub volume_id: String,
    pub size: u64,
    #[serde(skip)]
    pub inode: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DuplicateGroup {
    pub size: u64,
    pub hash: String,
    pub file_count: usize,
    pub reclaimable_bytes: u64,
    pub files: Vec<DuplicateFile>,
    pub members_truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunnelStage {
    pub stage: &'static str,
    pub files_in: u64,
    pub files_out: u64,
    pub eliminated: u64,
    pub bytes_read: u64,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DuplicateReport {
    pub total_online_files: u64,
    pub skipped_empty_files: u64,
    pub skipped_offline_files: u64,
    pub skipped_missing_files: u64,
    pub skipped_unreadable_files: u64,
    pub skipped_hardlink_rows: u64,
    pub candidate_size_groups: u64,
    pub candidate_files: u64,
    pub partial_hashed: u64,
    pub partial_bytes: u64,
    pub partial_eliminated: u64,
    pub full_hashed: u64,
    pub full_bytes: u64,
    pub full_eliminated: u64,
    /// Files whose full hash was already computed during the partial stage
    /// (whole-file reads) and did not need a second pass.
    pub full_reused: u64,
    pub duplicate_groups: u64,
    pub duplicate_files: u64,
    pub reclaimable_bytes: u64,
    pub naive_full_hash_bytes: u64,
    pub io_saved_bytes: u64,
    pub io_saved_ratio: f64,
    pub partial_bytes_setting: usize,
    pub took_ms: u64,
    pub funnel: Vec<FunnelStage>,
    pub groups: Vec<DuplicateGroup>,
    pub errors: Vec<String>,
}

struct Candidate {
    id: String,
    path: PathBuf,
    path_text: String,
    volume_id: String,
    size: u64,
    inode: u64,
}

/// Three-stage duplicate funnel:
/// 1. group by size (no content read),
/// 2. partial BLAKE3 over head+tail 4 KiB (only for size groups with >1 row),
/// 3. full BLAKE3 (only for partial-hash groups with >1 row).
///
/// Hardlinks are collapsed by (volume_id, inode) before any read, directories
/// and non-online rows (offline volume / missing) are excluded up front, and
/// zero-byte files are skipped because they reclaim nothing.
pub fn find_duplicates(conn: &Connection, options: &DuplicateOptions) -> Result<DuplicateReport> {
    let started = Instant::now();
    let partial_bytes = options.partial_bytes.clamp(512, 65536);
    let volume = options.volume_id.clone();
    let mut errors: Vec<String> = Vec::new();

    let volume_clause = " AND (?1 IS NULL OR f.volume_id = ?1)";

    // One pass for the online census (including the zero-byte count), one pass
    // for the non-online census. The old code ran four separate scans here;
    // on a cold 1M-row index that alone cost seconds.
    let (total_online_files, empty_files): (u64, u64) = conn.query_row(
        &format!(
            "SELECT COUNT(*), COALESCE(SUM(CASE WHEN f.size=0 THEN 1 ELSE 0 END),0)
             FROM files f JOIN volumes v ON v.id=f.volume_id AND v.status='online'
             WHERE f.is_dir=0 AND f.status='online'{}",
            volume_clause
        ),
        params![volume],
        |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
    )?;

    let (skipped_offline, skipped_missing): (u64, u64) = conn.query_row(
        &format!(
            "SELECT
                COALESCE(SUM(CASE WHEN f.status='offline' THEN 1 ELSE 0 END),0),
                COALESCE(SUM(CASE WHEN f.status='missing' THEN 1 ELSE 0 END),0)
             FROM files f WHERE f.is_dir=0 AND f.status<>'online'{}",
            volume_clause
        ),
        params![volume],
        |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
    )?;

    // Zero-byte files are pre-filtered out of the funnel by default: a cold
    // run over a tree full of them paid one open() per file to rediscover that
    // empty files carry no content. `include_empty` keeps them on purpose.
    let size_filter = if options.include_empty {
        ""
    } else {
        " AND f.size > 0"
    };

    // Size buckets. A full scan over `files` is the price of the first funnel
    // stage; the rows read here are only (size, count).
    let size_groups: Vec<(u64, u64)> = {
        let mut statement = conn.prepare(&format!(
            "SELECT f.size, COUNT(*) FROM files f
             JOIN volumes v ON v.id=f.volume_id AND v.status='online'
             WHERE f.is_dir=0 AND f.status='online'{} {}
             GROUP BY f.size HAVING COUNT(*)>1
             ORDER BY f.size * (COUNT(*)-1) DESC",
            volume_clause.trim_end(),
            size_filter
        ))?;
        let mapped = statement.query_map(params![volume], |row| {
            Ok((
                row.get::<_, i64>(0)?.max(0) as u64,
                row.get::<_, i64>(1)? as u64,
            ))
        })?;
        mapped.collect::<std::result::Result<Vec<_>, _>>()?
    };

    let skipped_empty_files = if options.include_empty {
        0
    } else {
        empty_files
    };

    let mut duplicate_groups: Vec<DuplicateGroup> = Vec::new();
    let mut partial_hashed = 0u64;
    let mut partial_bytes_read = 0u64;
    let mut partial_eliminated = 0u64;
    let mut full_hashed = 0u64;
    let mut full_bytes_read = 0u64;
    let mut full_reused = 0u64;
    let mut full_eliminated = 0u64;
    let mut skipped_hardlink_rows = 0u64;
    let mut skipped_unreadable_files = 0u64;
    let mut candidate_files = 0u64;
    let mut naive_full_hash_bytes = 0u64;
    let mut funnel: Vec<FunnelStage> = Vec::new();

    // Fetch candidates only for sizes that actually collide.
    let sizes: Vec<u64> = size_groups.iter().map(|(size, _)| *size).collect();
    if !sizes.is_empty() {
        let size_list = sizes
            .iter()
            .map(|size| size.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT f.id, f.path, f.size, f.inode, f.volume_id
             FROM files f JOIN volumes v ON v.id=f.volume_id AND v.status='online'
             WHERE f.is_dir=0 AND f.status='online' AND f.size IN ({size_list}){}
             ORDER BY f.size, f.path",
            volume_clause
        );
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(params![volume])?;
        let mut by_size: HashMap<u64, Vec<Candidate>> = HashMap::new();
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let path_text: String = row.get(1)?;
            let size: i64 = row.get(2)?;
            let inode: i64 = row.get(3)?;
            let volume_id: String = row.get(4)?;
            let size = size.max(0) as u64;
            candidate_files += 1;
            by_size.entry(size).or_default().push(Candidate {
                id,
                path: PathBuf::from(&path_text),
                path_text,
                volume_id,
                size,
                inode: inode.max(0) as u64,
            });
        }
        drop(rows);

        let mut size_stage_in = 0u64;
        let mut size_stage_out = 0u64;
        let mut unique_rows: Vec<(u64, Vec<Candidate>)> = Vec::new();
        for (size, candidates) in by_size {
            size_stage_in += candidates.len() as u64;
            let mut seen_inode: HashMap<(String, u64), ()> = HashMap::new();
            let mut kept = Vec::with_capacity(candidates.len());
            for candidate in candidates {
                if candidate.inode > 0 {
                    let key = (candidate.volume_id.clone(), candidate.inode);
                    if seen_inode.insert(key, ()).is_some() {
                        skipped_hardlink_rows += 1;
                        continue;
                    }
                }
                kept.push(candidate);
            }
            if kept.len() > 1 {
                naive_full_hash_bytes += size * kept.len() as u64;
                size_stage_out += kept.len() as u64;
                unique_rows.push((size, kept));
            }
        }
        funnel.push(FunnelStage {
            stage: "size_group",
            files_in: size_stage_in,
            files_out: size_stage_out,
            eliminated: size_stage_in - size_stage_out,
            bytes_read: 0,
            elapsed_ms: 0,
        });

        // Stage 2: partial hash.
        let partial_started = Instant::now();
        let mut partial_groups: Vec<PartialGroup> = Vec::new();
        for (size, candidates) in unique_rows {
            let mut groups: HashMap<String, Vec<(Candidate, Option<String>)>> = HashMap::new();
            for candidate in candidates {
                partial_hashed += 1;
                match partial_hash(&candidate.path, size, partial_bytes) {
                    Ok(digest) => {
                        partial_bytes_read += digest.bytes_read;
                        groups
                            .entry(digest.digest)
                            .or_default()
                            .push((candidate, digest.full_digest));
                    }
                    Err(error) => {
                        skipped_unreadable_files += 1;
                        if errors.len() < 20 {
                            errors.push(format!("{}: {error}", candidate.path_text));
                        }
                    }
                }
            }
            for (_digest, members) in groups {
                if members.len() > 1 {
                    partial_groups.push(PartialGroup { size, members });
                } else {
                    partial_eliminated += members.len() as u64;
                }
            }
        }
        funnel.push(FunnelStage {
            stage: "partial_hash",
            files_in: partial_hashed,
            files_out: partial_groups
                .iter()
                .map(|group| group.members.len() as u64)
                .sum(),
            eliminated: partial_eliminated,
            bytes_read: partial_bytes_read,
            elapsed_ms: partial_started.elapsed().as_millis() as u64,
        });

        // Stage 3: full hash.
        let full_started = Instant::now();
        let mut full_entered = 0u64;
        for group in partial_groups {
            let size = group.size;
            let mut groups: HashMap<String, Vec<Candidate>> = HashMap::new();
            for (candidate, known_full) in group.members {
                full_hashed += 1;
                full_entered += 1;
                if let Some(hash) = known_full {
                    full_reused += 1;
                    groups.entry(hash).or_default().push(candidate);
                    continue;
                }
                match full_hash(&candidate.path) {
                    Ok((hash, bytes)) => {
                        full_bytes_read += bytes;
                        groups.entry(hash).or_default().push(candidate);
                    }
                    Err(error) => {
                        skipped_unreadable_files += 1;
                        if errors.len() < 20 {
                            errors.push(format!("{}: {error}", candidate.path_text));
                        }
                    }
                }
            }
            for (hash, members) in groups {
                if members.len() > 1 {
                    let reclaimable = size * (members.len() as u64 - 1);
                    duplicate_groups.push(DuplicateGroup {
                        size,
                        hash,
                        file_count: members.len(),
                        reclaimable_bytes: reclaimable,
                        files: members
                            .iter()
                            .take(100)
                            .map(|candidate| DuplicateFile {
                                id: candidate.id.clone(),
                                path: candidate.path_text.clone(),
                                volume_id: candidate.volume_id.clone(),
                                size: candidate.size,
                                inode: candidate.inode,
                            })
                            .collect(),
                        members_truncated: members.len() > 100,
                    });
                } else {
                    full_eliminated += members.len() as u64;
                }
            }
        }
        funnel.push(FunnelStage {
            stage: "full_hash",
            files_in: full_entered,
            files_out: full_entered - full_eliminated,
            eliminated: full_eliminated,
            bytes_read: full_bytes_read,
            elapsed_ms: full_started.elapsed().as_millis() as u64,
        });
    }

    duplicate_groups.sort_by_key(|group| std::cmp::Reverse(group.reclaimable_bytes));
    let duplicate_files: u64 = duplicate_groups
        .iter()
        .map(|group| group.file_count as u64 - 1)
        .sum();
    let reclaimable_bytes: u64 = duplicate_groups
        .iter()
        .map(|group| group.reclaimable_bytes)
        .sum();
    let total_groups = duplicate_groups.len() as u64;
    duplicate_groups.truncate(options.max_groups);

    let io_saved_bytes = naive_full_hash_bytes.saturating_sub(partial_bytes_read + full_bytes_read);
    Ok(DuplicateReport {
        total_online_files,
        skipped_empty_files,
        skipped_offline_files: skipped_offline,
        skipped_missing_files: skipped_missing,
        skipped_unreadable_files,
        skipped_hardlink_rows,
        candidate_size_groups: size_groups.len() as u64,
        candidate_files,
        partial_hashed,
        partial_bytes: partial_bytes_read,
        partial_eliminated,
        full_hashed,
        full_bytes: full_bytes_read,
        full_eliminated,
        full_reused,
        duplicate_groups: total_groups,
        duplicate_files,
        reclaimable_bytes,
        naive_full_hash_bytes,
        io_saved_bytes,
        io_saved_ratio: if naive_full_hash_bytes == 0 {
            0.0
        } else {
            io_saved_bytes as f64 / naive_full_hash_bytes as f64
        },
        partial_bytes_setting: partial_bytes,
        took_ms: started.elapsed().as_millis() as u64,
        funnel,
        groups: duplicate_groups,
        errors,
    })
}

struct PartialGroup {
    size: u64,
    members: Vec<(Candidate, Option<String>)>,
}

struct PartialDigest {
    digest: String,
    /// Set when the file was small enough to be read whole: that digest is
    /// already the full-content hash, so stage 3 can skip the second read.
    full_digest: Option<String>,
    bytes_read: u64,
}

/// BLAKE3 over head+tail. Files smaller than two windows are read once in
/// full, which avoids a seek that would cost more than the read itself and
/// yields the full-content hash for free.
fn partial_hash(
    path: &std::path::Path,
    size: u64,
    window: usize,
) -> std::io::Result<PartialDigest> {
    let mut file = File::open(path)?;
    if size <= (window * 2) as u64 {
        let mut buffer = Vec::with_capacity(size as usize);
        file.read_to_end(&mut buffer)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"whole");
        hasher.update(&buffer);
        return Ok(PartialDigest {
            digest: hasher.finalize().to_hex().to_string(),
            full_digest: Some(blake3::hash(&buffer).to_hex().to_string()),
            bytes_read: buffer.len() as u64,
        });
    }
    let mut head = vec![0u8; window];
    file.read_exact(&mut head)?;
    let mut tail = vec![0u8; window];
    file.seek(SeekFrom::End(-(window as i64)))?;
    file.read_exact(&mut tail)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"head+tail");
    hasher.update(&head);
    hasher.update(&tail);
    Ok(PartialDigest {
        digest: hasher.finalize().to_hex().to_string(),
        full_digest: None,
        bytes_read: (head.len() + tail.len()) as u64,
    })
}

fn full_hash(path: &std::path::Path) -> std::io::Result<(String, u64)> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total += read as u64;
    }
    Ok((hasher.finalize().to_hex().to_string(), total))
}
