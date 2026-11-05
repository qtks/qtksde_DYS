use std::env;
use std::path::PathBuf;
use std::time::Duration;

use walker_index::{index_root, poll_volume, IndexDb, IndexOptions, IndexStats, VolumeEvent};

const HELP: &str = "\
walker-index - Panopticon M1 SQLite metadata index

USAGE:
    walker-index scan   --db FILE [PATH] [OPTIONS]
    walker-index watch  --db FILE --mount PATH [--interval-secs N] [--once]
    walker-index status --db FILE

SCAN OPTIONS:
    --db FILE                     SQLite database (created if missing).
    PATH                          Volume root to index. Defaults to $HOME.
    --batch-size N                Entries per transaction (default 5000).
    -e, --exclude PATTERN         Extra exclusion rule (repeatable).
        --no-default-excludes     Disable /proc, /sys, node_modules, .git/objects.
        --follow-links            Follow directory symlinks with cycle detection.
        --threads N               Restrict rayon to N threads.
        --no-resume               Ignore an interrupted session and start fresh.
        --abort-after-batches N   Fault injection: stop after N committed batches.
        --fail-during-batch-after N
                                  Fault injection: abort the first batch after N rows.
        --json                    Print a machine-readable summary.

WATCH OPTIONS:
    --mount PATH                  Mount point to poll.
    --interval-secs N             Poll interval (default 5).
    --once                        Poll once and exit.

    -h, --help                    Show this help.
";

fn main() {
    if let Err(message) = run() {
        eprintln!("walker-index: {message}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        print!("{HELP}");
        return Ok(());
    };
    let rest: Vec<String> = args.collect();
    match command.as_str() {
        "-h" | "--help" | "help" => {
            print!("{HELP}");
            Ok(())
        }
        "scan" => command_scan(&rest),
        "watch" => command_watch(&rest),
        "status" => command_status(&rest),
        other => Err(format!("unknown subcommand: {other} (try --help)")),
    }
}

fn command_scan(arguments: &[String]) -> Result<(), String> {
    let mut db_path: Option<PathBuf> = None;
    let mut root: Option<PathBuf> = None;
    let mut batch_size = 5000usize;
    let mut extra_excludes = Vec::new();
    let mut use_default_excludes = true;
    let mut follow_links = false;
    let mut resume = true;
    let mut threads = None;
    let mut abort_after_batches = None;
    let mut fail_during_batch_after = None;
    let mut json_output = false;

    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        index += 1;
        match argument {
            "--db" => db_path = Some(PathBuf::from(take_value(arguments, &mut index, "--db")?)),
            "--batch-size" => {
                batch_size = parse_usize(take_value(arguments, &mut index, "--batch-size")?)?;
                if batch_size == 0 {
                    return Err("--batch-size must be at least 1".to_string());
                }
            }
            "-e" | "--exclude" => {
                extra_excludes.push(take_value(arguments, &mut index, "--exclude")?)
            }
            "--no-default-excludes" => use_default_excludes = false,
            "--follow-links" => follow_links = true,
            "--no-resume" => resume = false,
            "--threads" => {
                threads = Some(parse_usize(take_value(
                    arguments,
                    &mut index,
                    "--threads",
                )?)?)
            }
            "--abort-after-batches" => {
                abort_after_batches = Some(parse_u64(take_value(
                    arguments,
                    &mut index,
                    "--abort-after-batches",
                )?)?)
            }
            "--fail-during-batch-after" => {
                fail_during_batch_after = Some(parse_u64(take_value(
                    arguments,
                    &mut index,
                    "--fail-during-batch-after",
                )?)?)
            }
            "--json" => json_output = true,
            other if other.starts_with('-') => return Err(format!("unknown option: {other}")),
            other => root = Some(PathBuf::from(other)),
        }
    }

    let db_path = db_path.ok_or_else(|| "--db is required".to_string())?;
    let root = root.unwrap_or_else(default_root);

    let options = IndexOptions {
        db_path,
        root,
        extra_excludes,
        use_default_excludes,
        follow_links,
        batch_size,
        resume,
        abort_after_batches,
        fail_during_batch_after,
        threads,
    };
    let outcome = index_root(&options).map_err(|error| error.to_string())?;
    print_outcome(&outcome, json_output)
}

fn print_outcome(outcome: &walker_index::IndexOutcome, json_output: bool) -> Result<(), String> {
    let stats = &outcome.stats;
    if json_output {
        let payload = serde_json::json!({
            "volume_id": outcome.volume.id,
            "mount_path": outcome.volume.mount_path.to_string_lossy(),
            "session_id": outcome.session_id,
            "entries": stats.entries_total,
            "inserted": stats.inserted,
            "updated": stats.updated,
            "unchanged": stats.unchanged,
            "missing": stats.marked_missing,
            "cascaded_missing": stats.cascaded_missing,
            "rows_written": stats.rows_written,
            "batches": stats.batches_committed,
            "resumed": stats.resumed,
            "resumed_from_batch": stats.resumed_from_batch,
            "restored_offline_rows": stats.restored_offline_rows,
            "traversal_ms": stats.traversal_ms,
            "serialize_ms": stats.serialize_ms,
            "write_ms": stats.write_ms,
            "index_build_ms": stats.index_build_ms,
            "restore_ms": stats.restore_ms,
            "total_ms": stats.total_ms,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).map_err(|error| error.to_string())?
        );
        return Ok(());
    }

    if let Some(report) = &outcome.report {
        println!(
            "traversed {} entries ({} files, {} dirs, {} symlinks) from {}",
            report.summary.files + report.summary.directories + report.summary.symlinks,
            report.summary.files,
            report.summary.directories,
            report.summary.symlinks,
            outcome.volume.mount_path.display()
        );
    }
    print_summary(
        stats,
        outcome.session_id.as_str(),
        outcome.volume.status.as_str(),
    );
    Ok(())
}

fn print_summary(stats: &IndexStats, session_id: &str, volume_status: &str) {
    println!(
        "indexed {} entries in {} ms (traversal {} ms, serialize {} ms, write {} ms \
         [index build {} ms], restore {} ms)",
        stats.entries_total,
        stats.total_ms,
        stats.traversal_ms,
        stats.serialize_ms,
        stats.write_ms,
        stats.index_build_ms,
        stats.restore_ms
    );
    println!(
        "rows: inserted {} | updated {} | unchanged {} | missing {} | cascaded {} | batches {}",
        stats.inserted,
        stats.updated,
        stats.unchanged,
        stats.marked_missing,
        stats.cascaded_missing,
        stats.batches_committed
    );
    if stats.resumed {
        println!(
            "resumed session {session_id} from batch {}",
            stats.resumed_from_batch
        );
    } else {
        println!("session {session_id} completed");
    }
    println!("volume status: {volume_status}");
}

fn command_watch(arguments: &[String]) -> Result<(), String> {
    let mut db_path: Option<PathBuf> = None;
    let mut mount: Option<PathBuf> = None;
    let mut interval_seconds = 5u64;
    let mut once = false;

    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        index += 1;
        match argument {
            "--db" => db_path = Some(PathBuf::from(take_value(arguments, &mut index, "--db")?)),
            "--mount" => mount = Some(PathBuf::from(take_value(arguments, &mut index, "--mount")?)),
            "--interval-secs" => {
                interval_seconds = parse_u64(take_value(arguments, &mut index, "--interval-secs")?)?
            }
            "--once" => once = true,
            other => return Err(format!("unknown watch option: {other}")),
        }
    }

    let db_path = db_path.ok_or_else(|| "--db is required".to_string())?;
    let mount = mount.ok_or_else(|| "--mount is required".to_string())?;
    let mut db = IndexDb::open(&db_path).map_err(|error| error.to_string())?;

    loop {
        let event = poll_volume(&mut db, &mount).map_err(|error| error.to_string())?;
        match event {
            VolumeEvent::NotIndexed { mount_path } => {
                println!("{mount_path}: not indexed yet")
            }
            VolumeEvent::Unchanged { volume } => {
                println!("{}: unchanged ({} )", volume.mount_path.display(), volume.status.as_str())
            }
            VolumeEvent::WentOffline {
                volume_id,
                files_marked_offline,
                reason,
            } => println!(
                "volume {volume_id} went offline ({reason}); {files_marked_offline} rows marked offline"
            ),
            VolumeEvent::CameOnline {
                volume_id,
                files_restored,
            } => println!(
                "volume {volume_id} is back; {files_restored} rows restored"
            ),
        }
        if once {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(interval_seconds.max(1)));
    }
}

fn command_status(arguments: &[String]) -> Result<(), String> {
    let mut db_path: Option<PathBuf> = None;
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        index += 1;
        match argument {
            "--db" => db_path = Some(PathBuf::from(take_value(arguments, &mut index, "--db")?)),
            other => return Err(format!("unknown status option: {other}")),
        }
    }
    let db_path = db_path.ok_or_else(|| "--db is required".to_string())?;
    let db = IndexDb::open(&db_path).map_err(|error| error.to_string())?;

    for volume in db.volumes().map_err(|error| error.to_string())? {
        println!(
            "volume {id} {mount} status={status} device={device} seen={seen}",
            id = volume.id,
            mount = volume.mount_path.display(),
            status = volume.status.as_str(),
            device = volume
                .device
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_string()),
            seen = volume.last_seen_at
        );
        for (kind, count) in db
            .file_counts_by_type(&volume.id)
            .map_err(|error| error.to_string())?
        {
            println!("  {kind}: {count}");
        }
        for (status, count) in db
            .file_counts_by_status(&volume.id)
            .map_err(|error| error.to_string())?
        {
            println!("  status {status}: {count}");
        }
        let session: Option<(String, String, i64, i64)> = db
            .connection()
            .query_row(
                "SELECT id, status, completed_batches, total_entries FROM scan_sessions
                 WHERE volume_id=?1 ORDER BY started_at DESC, rowid DESC LIMIT 1",
                [&volume.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .ok();
        if let Some((id, status, batches, total)) = session {
            println!("  session {id} {status} batches={batches} entries={total}");
        }
    }
    Ok(())
}

fn take_value(arguments: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    let value = arguments
        .get(*index)
        .ok_or_else(|| format!("{flag} requires a value"))?;
    *index += 1;
    Ok(value.clone())
}

fn parse_usize(value: String) -> Result<usize, String> {
    value
        .parse()
        .map_err(|_| format!("invalid number: {value}"))
}

fn parse_u64(value: String) -> Result<u64, String> {
    value
        .parse()
        .map_err(|_| format!("invalid number: {value}"))
}

fn default_root() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}
