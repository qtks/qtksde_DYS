use std::collections::HashMap;
use std::fs;
use std::io::Seek;
use std::time::Instant;

use walker_core::{scan_with_entries, ScanOptions, ScanOutcome};

use crate::db::{BatchContext, IndexDb};
use crate::error::IndexError;
use crate::model::{IndexOptions, IndexOutcome, IndexStats, ScanSession, SessionPhase};
use crate::timeutil::now_iso8601;
use crate::work::{open_batch_reader, read_batch, read_listed_directories, WorkFiles};

/// Index (or re-index) a single volume root.
///
/// Pipeline:
/// 1. `collecting` — parallel metadata traversal into an in-memory entry list.
/// 2. `serialization` — sort entries by materialized path, write a JSONL work
///    file and the list of successfully listed directories.
/// 3. `writing` — read the work file in 5000-entry batches; each batch is one
///    SQLite transaction that also advances the checkpoint row.
///
/// Crashes during `writing` resume at the last committed batch. Crashes during
/// `collecting` re-run the traversal (the DB has not been touched yet).
pub fn index_root(options: &IndexOptions) -> Result<IndexOutcome, IndexError> {
    let total_start = Instant::now();
    let batch_size = options.batch_size.max(1);

    if options.db_path.as_os_str().is_empty() {
        return Err(IndexError::Config("db_path is required".to_string()));
    }
    if options.root.as_os_str().is_empty() {
        return Err(IndexError::Config("root is required".to_string()));
    }

    // Canonical paths are what the walker emits (symlinked roots included).
    let canonical_root = fs::canonicalize(&options.root).unwrap_or_else(|_| options.root.clone());
    let root_path = canonical_root.to_string_lossy().into_owned();

    let device = {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(&canonical_root)
            .ok()
            .map(|metadata| metadata.dev())
    };

    let mut db = IndexDb::open(&options.db_path)?;
    let now = now_iso8601();
    let (volume, was_offline) = db.attach_volume(&root_path, device)?;

    let mut stats = IndexStats::default();
    if was_offline {
        let restore_start = Instant::now();
        stats.restored_offline_rows = db.restore_offline_files(&volume.id, &now)?;
        stats.restore_ms = elapsed_ms(restore_start);
    }

    let existing = if options.resume {
        db.latest_running_session(&volume.id)?
    } else {
        db.abort_running_sessions(&volume.id)?;
        None
    };

    let mut resumeable = false;
    let mut session = match existing {
        Some(session) => {
            let files_present = session
                .work_file
                .as_ref()
                .map(|path| path.exists())
                .unwrap_or(false)
                && session
                    .dirs_file
                    .as_ref()
                    .map(|path| path.exists())
                    .unwrap_or(false);
            if session.phase == SessionPhase::Writing && files_present {
                resumeable = true;
                stats.resumed = true;
                stats.resumed_from_batch = session.completed_batches;
                stats.entries_total = session.total_entries;
                session
            } else {
                db.reset_session_collecting(&session.id)?;
                session
            }
        }
        None => db.create_session(&volume.id, &root_path, batch_size)?,
    };

    let mut report = None;
    // Fresh collections keep the sorted entries in memory so the writer does
    // not have to parse 100+ MB of JSONL back. The file is still written for
    // crash resume and its batch offsets feed the checkpoint column.
    let mut in_memory: Option<InMemoryBatches> = None;
    if !resumeable {
        let scan_options = ScanOptions {
            roots: vec![canonical_root],
            extra_excludes: options.extra_excludes.clone(),
            use_default_excludes: options.use_default_excludes,
            follow_links: options.follow_links,
            threads: options.threads,
            ..ScanOptions::default()
        };
        let collect_start = Instant::now();
        let outcome = scan_with_entries(&scan_options);
        stats.traversal_ms = elapsed_ms(collect_start);

        let ScanOutcome {
            report: scan_report,
            entries,
            listed_directories,
        } = outcome;
        stats.entries_total = entries.len() as u64;

        let serialize_start = Instant::now();
        let work = WorkFiles::new(&options.db_path, &session.id)?;
        let mut entries = crate::db::work_entries(entries);
        entries.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        let offsets = work.write(&entries, &listed_directories, batch_size)?;
        stats.serialize_ms = elapsed_ms(serialize_start);
        in_memory = Some(InMemoryBatches { entries, offsets });

        let work_file = work.jsonl.to_string_lossy().into_owned();
        let dirs_file = work.dirs.to_string_lossy().into_owned();
        db.mark_session_writing(&session.id, stats.entries_total, &work_file, &dirs_file)?;

        session.phase = SessionPhase::Writing;
        session.work_file = Some(work.jsonl);
        session.dirs_file = Some(work.dirs);
        session.work_file_offset = 0;
        session.completed_batches = 0;
        session.cursor_path = None;
        session.total_entries = stats.entries_total;
        report = Some(scan_report);
    }

    let bulk_load = !resumeable && db.volume_row_count(&volume.id)? == 0;
    if bulk_load {
        db.drop_secondary_indexes()?;
    }

    let job = WriteJob {
        volume_id: &volume.id,
        root_path: &root_path,
        session: &session,
        now: &now,
        options,
    };
    write_batches(&mut db, &job, &mut stats, total_start, in_memory.as_ref())?;

    if bulk_load {
        let build_start = Instant::now();
        db.rebuild_secondary_indexes()?;
        stats.index_build_ms = elapsed_ms(build_start);
        stats.write_ms += stats.index_build_ms;
    }

    let checkpoint_start = Instant::now();
    db.checkpoint()?;
    stats.write_ms += elapsed_ms(checkpoint_start);
    stats.total_ms = elapsed_ms(total_start);

    // The work files only exist to survive a crash between batches; once the
    // session is committed they are dead weight (~100 MB per 1M entries).
    for path in [session.work_file.as_ref(), session.dirs_file.as_ref()]
        .into_iter()
        .flatten()
    {
        let _ = fs::remove_file(path);
    }

    Ok(IndexOutcome {
        volume,
        session_id: session.id,
        report,
        stats,
    })
}

struct WriteJob<'a> {
    volume_id: &'a str,
    root_path: &'a str,
    session: &'a ScanSession,
    now: &'a str,
    options: &'a IndexOptions,
}

/// Sorted entries plus the byte offset that starts each batch in the work
/// file. Used for the collecting-then-writing fast path.
struct InMemoryBatches {
    entries: Vec<crate::work::WorkEntry>,
    offsets: Vec<u64>,
}

/// One batch, either borrowed from the in-memory entry list or read back from
/// the work file after a resume.
enum BatchSlices<'a> {
    Borrowed(&'a [crate::work::WorkEntry]),
    Owned(Vec<crate::work::WorkEntry>),
}

impl BatchSlices<'_> {
    fn as_slice(&self) -> &[crate::work::WorkEntry] {
        match self {
            Self::Borrowed(entries) => entries,
            Self::Owned(entries) => entries,
        }
    }
}

fn write_batches(
    db: &mut IndexDb,
    job: &WriteJob<'_>,
    stats: &mut IndexStats,
    total_start: Instant,
    in_memory: Option<&InMemoryBatches>,
) -> Result<(), IndexError> {
    let session = job.session;
    let options = job.options;
    let volume_id = job.volume_id;
    let root_path = job.root_path;
    let now = job.now;
    let work_file = session
        .work_file
        .clone()
        .ok_or_else(|| IndexError::Config("session has no work file".to_string()))?;
    let dirs_file = session
        .dirs_file
        .clone()
        .ok_or_else(|| IndexError::Config("session has no directory list".to_string()))?;

    let dirs_listed = read_listed_directories(&dirs_file)?;
    let mut dir_ids: HashMap<String, String> = HashMap::new();
    if stats.resumed {
        for (path, id) in db.directory_ids(volume_id)? {
            dir_ids.insert(path, id);
        }
    }

    let mut reader = match in_memory {
        Some(_) => None,
        None => Some(open_batch_reader(&work_file, session.work_file_offset)?),
    };
    let mut cursor = session.cursor_path.clone();
    let batch_size = session.batch_size.max(1) as usize;
    let mut batch_index = session.completed_batches;
    let write_start = Instant::now();

    loop {
        let (batch, new_offset): (BatchSlices<'_>, u64) = match in_memory {
            Some(memory) => {
                let start = batch_index as usize * batch_size;
                if start >= memory.entries.len() {
                    break;
                }
                let end = (start + batch_size).min(memory.entries.len());
                let offset = memory
                    .offsets
                    .get(batch_index as usize + 1)
                    .copied()
                    .unwrap_or_else(|| memory.offsets.last().copied().unwrap_or(0));
                (BatchSlices::Borrowed(&memory.entries[start..end]), offset)
            }
            None => {
                let source = reader.as_mut().expect("reader");
                let owned = read_batch(source, batch_size)?;
                if owned.is_empty() {
                    break;
                }
                let position = source.stream_position()?;
                (BatchSlices::Owned(owned), position)
            }
        };
        let entries = batch.as_slice();
        batch_index += 1;

        let mut context = BatchContext {
            volume_id,
            root_path,
            session_id: &session.id,
            dirs_listed: &dirs_listed,
            dir_ids: &mut dir_ids,
            cursor: &mut cursor,
            batch_index,
            new_offset,
            now,
            fail_after_rows: options.fail_during_batch_after,
        };
        let batch = db.apply_batch(entries, &mut context)?;
        stats.inserted += batch.inserted;
        stats.updated += batch.updated;
        stats.unchanged += batch.unchanged;
        stats.marked_missing += batch.marked_missing;
        stats.cascaded_missing += batch.cascaded_missing;
        stats.rows_written += batch.rows_written;

        if let Some(limit) = options.abort_after_batches {
            if batch_index >= limit {
                stats.write_ms = elapsed_ms(write_start);
                stats.batches_committed = batch_index;
                stats.total_ms = elapsed_ms(total_start);
                return Err(IndexError::Aborted {
                    batches: batch_index,
                });
            }
        }
    }

    // Rows that never appeared in this scan. Only those whose parent directory
    // was fully listed are marked missing, so unreadable directories do not
    // masquerade as deletions.
    loop {
        let cleanup = db.cleanup_chunk(
            volume_id,
            root_path,
            &session.id,
            &dirs_listed,
            &mut cursor,
            now,
        )?;
        stats.marked_missing += cleanup.marked;
        stats.cascaded_missing += cleanup.cascaded;
        stats.rows_written += cleanup.marked + cleanup.cascaded;
        if cleanup.rows == 0 {
            break;
        }
    }

    db.finish_session(&session.id, stats.entries_total)?;

    let write_ms = elapsed_ms(write_start);
    stats.write_ms = write_ms;
    stats.batches_committed = batch_index;
    stats.total_ms = elapsed_ms(total_start);
    Ok(())
}

fn elapsed_ms(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}
