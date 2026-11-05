use std::env;
use std::path::PathBuf;
use std::time::Instant;

use rusqlite::Connection;
use walker_query::{
    aggregate_dir_totals, child_rows, find_duplicates, fts_status, open_query_connection,
    open_write_connection, rebuild_fts, search, serve, treemap_rows, DuplicateOptions, HttpOptions,
    PrefixMode, SearchKind, SearchOptions, SearchResult,
};

const HELP: &str = "\
walker-query - Panopticon M2 query layer

USAGE:
    walker-query search     --db FILE (--exact|--prefix|--fts) --q TEXT [OPTIONS]
    walker-query bench      --db FILE --q TEXT [--iterations N] [--json]
    walker-query fts-build  --db FILE [--json]
    walker-query fts-status --db FILE [--json]
    walker-query aggregate  --db FILE [--json]
    walker-query treemap    --db FILE [--path DIR] [--limit N] [--json]
    walker-query children   --db FILE --path DIR [--limit N] [--json]
    walker-query dup        --db FILE [--partial-bytes N] [--limit N] [--json]
    walker-query serve      --db FILE [--addr HOST:PORT] [--ui-dir DIR] [--bench-dir DIR]

SEARCH OPTIONS:
    --kind exact|prefix|fts       Match mode (also --exact/--prefix/--fts shortcuts).
    --q TEXT                      Query string (normalized to NFC before matching).
    --limit N                     Max hits (default 200).
    --offset N                    Skip hits (default 0).
    --prefix-mode range|like      Prefix access path (default range).
    --volume ID                   Restrict to one volume.
    --no-rank                     FTS: skip bm25 ordering so LIMIT can stop early (default).
    --rank                        FTS: order by bm25, scores every match first.
    --json                        Print the full JSON result including EXPLAIN plan.

    -h, --help                    Show this help.
";

fn main() {
    if let Err(message) = run() {
        eprintln!("walker-query: {message}");
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
        "search" => command_search(&rest),
        "bench" => command_bench(&rest),
        "fts-build" => command_fts_build(&rest),
        "fts-status" => command_fts_status(&rest),
        "aggregate" => command_aggregate(&rest),
        "treemap" => command_treemap(&rest),
        "children" => command_children(&rest),
        "dup" => command_dup(&rest),
        "serve" => command_serve(&rest),
        other => Err(format!("unknown subcommand: {other} (try --help)")),
    }
}

struct SearchArgs {
    db: PathBuf,
    kind: SearchKind,
    query: String,
    limit: usize,
    offset: usize,
    prefix_mode: PrefixMode,
    volume: Option<String>,
    rank: bool,
    json: bool,
}

fn parse_search_args(arguments: &[String]) -> Result<SearchArgs, String> {
    let mut db: Option<PathBuf> = None;
    let mut kind: Option<SearchKind> = None;
    let mut query: Option<String> = None;
    let mut limit = 200usize;
    let mut offset = 0usize;
    let mut prefix_mode = PrefixMode::Range;
    let mut volume = None;
    let mut rank = false;
    let mut json = false;

    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        index += 1;
        match argument {
            "--db" => db = Some(PathBuf::from(take_value(arguments, &mut index, "--db")?)),
            "--kind" => {
                let value = take_value(arguments, &mut index, "--kind")?;
                kind = Some(
                    SearchKind::parse(&value).ok_or_else(|| format!("unknown kind: {value}"))?,
                );
            }
            "--exact" => kind = Some(SearchKind::Exact),
            "--prefix" => kind = Some(SearchKind::Prefix),
            "--fts" => kind = Some(SearchKind::Fts),
            "--q" => query = Some(take_value(arguments, &mut index, "--q")?),
            "--limit" => limit = parse_usize(take_value(arguments, &mut index, "--limit")?)?,
            "--offset" => offset = parse_usize(take_value(arguments, &mut index, "--offset")?)?,
            "--prefix-mode" => {
                let value = take_value(arguments, &mut index, "--prefix-mode")?;
                prefix_mode = PrefixMode::parse(&value)
                    .ok_or_else(|| format!("unknown prefix mode: {value}"))?;
            }
            "--volume" => volume = Some(take_value(arguments, &mut index, "--volume")?),
            "--no-rank" => rank = false,
            "--rank" => rank = true,
            "--json" => json = true,
            other => return Err(format!("unknown search option: {other}")),
        }
    }

    Ok(SearchArgs {
        db: db.ok_or_else(|| "--db is required".to_string())?,
        kind: kind.ok_or_else(|| "one of --exact/--prefix/--fts is required".to_string())?,
        query: query.ok_or_else(|| "--q is required".to_string())?,
        limit,
        offset,
        prefix_mode,
        volume,
        rank,
        json,
    })
}

fn command_search(arguments: &[String]) -> Result<(), String> {
    let args = parse_search_args(arguments)?;
    let conn = open_query_connection(&args.db).map_err(|error| error.to_string())?;
    let result = search(
        &conn,
        &SearchOptions {
            kind: args.kind,
            query: args.query,
            limit: args.limit,
            prefix_mode: args.prefix_mode,
            volume_id: args.volume,
            offset: args.offset,
            rank: args.rank,
        },
    )
    .map_err(|error| error.to_string())?;

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&result).map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    print_search_summary(&result);
    for hit in result.hits.iter().take(20) {
        println!(
            "{:>10}  {}{}",
            hit.size,
            hit.path,
            if hit.is_dir { "/" } else { "" }
        );
    }
    if result.hits.len() > 20 {
        println!("... {} more", result.hits.len() - 20);
    }
    Ok(())
}

fn print_search_summary(result: &SearchResult) {
    println!(
        "{} hits ({}) in {} us via {}  [query {:?}]",
        result.hits.len(),
        if result.hits.len() >= result.limit {
            "limit reached"
        } else {
            "all matches"
        },
        result.took_us,
        result.strategy,
        result.norm_query
    );
    if let Some(note) = &result.note {
        println!("note: {note}");
    }
    for line in &result.plan {
        println!("  plan: {line}");
    }
}

fn command_bench(arguments: &[String]) -> Result<(), String> {
    let mut db: Option<PathBuf> = None;
    let mut query: Option<String> = None;
    let mut limit = 200usize;
    let mut iterations = 30usize;
    let mut json = false;
    let mut compare_prefix_modes = false;
    let mut rank = false;
    let mut kinds = vec![SearchKind::Exact, SearchKind::Prefix, SearchKind::Fts];

    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        index += 1;
        match argument {
            "--db" => db = Some(PathBuf::from(take_value(arguments, &mut index, "--db")?)),
            "--q" => query = Some(take_value(arguments, &mut index, "--q")?),
            "--limit" => limit = parse_usize(take_value(arguments, &mut index, "--limit")?)?,
            "--iterations" => {
                iterations = parse_usize(take_value(arguments, &mut index, "--iterations")?)?
            }
            "--kinds" => {
                let value = take_value(arguments, &mut index, "--kinds")?;
                let mut parsed = Vec::new();
                for item in value.split(',') {
                    parsed.push(
                        SearchKind::parse(item).ok_or_else(|| format!("unknown kind: {item}"))?,
                    );
                }
                kinds = parsed;
            }
            "--compare-prefix-modes" => compare_prefix_modes = true,
            "--no-rank" => rank = false,
            "--rank" => rank = true,
            "--json" => json = true,
            other => return Err(format!("unknown bench option: {other}")),
        }
    }
    let db = db.ok_or_else(|| "--db is required".to_string())?;
    let query = query.ok_or_else(|| "--q is required".to_string())?;
    let conn = open_query_connection(&db).map_err(|error| error.to_string())?;

    let mut cases = Vec::new();
    for kind in kinds {
        cases.push(run_case(
            &conn,
            &query,
            kind,
            PrefixMode::Range,
            limit,
            iterations,
            rank,
        )?);
        if compare_prefix_modes && kind == SearchKind::Prefix {
            cases.push(run_case(
                &conn,
                &query,
                kind,
                PrefixMode::Like,
                limit,
                iterations,
                rank,
            )?);
        }
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&cases).map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    println!(
        "{:<8} {:<10} {:>10} {:>10} {:>10} {:>10} {:>8}",
        "kind", "strategy", "p50_us", "p95_us", "min_us", "max_us", "hits"
    );
    for case in &cases {
        println!(
            "{:<8} {:<10} {:>10} {:>10} {:>10} {:>10} {:>8}",
            case.kind, case.strategy, case.p50_us, case.p95_us, case.min_us, case.max_us, case.hits
        );
    }
    println!("iterations={iterations} (3 warmup), cache=warm (OS page cache), limit={limit}");
    Ok(())
}

#[derive(serde::Serialize)]
struct BenchCase {
    kind: String,
    strategy: String,
    prefix_mode: String,
    query: String,
    limit: usize,
    iterations: usize,
    rank: bool,
    hits: usize,
    p50_us: u64,
    p95_us: u64,
    min_us: u64,
    max_us: u64,
}

fn run_case(
    conn: &Connection,
    query: &str,
    kind: SearchKind,
    prefix_mode: PrefixMode,
    limit: usize,
    iterations: usize,
    rank: bool,
) -> Result<BenchCase, String> {
    let options = SearchOptions {
        kind,
        query: query.to_string(),
        limit,
        prefix_mode,
        volume_id: None,
        offset: 0,
        rank,
    };
    let mut strategy = String::new();
    let mut hits = 0usize;
    for _ in 0..3 {
        let result = search(conn, &options).map_err(|error| error.to_string())?;
        strategy = result.strategy;
        hits = result.hits.len();
    }
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        search(conn, &options).map_err(|error| error.to_string())?;
        samples.push(started.elapsed().as_micros() as u64);
    }
    samples.sort_unstable();
    let pick = |percent: f64| -> u64 {
        if samples.is_empty() {
            return 0;
        }
        let index = ((samples.len() as f64) * percent).ceil() as usize;
        samples[index.saturating_sub(1).min(samples.len() - 1)]
    };
    Ok(BenchCase {
        kind: format!("{kind:?}").to_lowercase(),
        strategy,
        prefix_mode: format!("{prefix_mode:?}").to_lowercase(),
        query: query.to_string(),
        limit,
        iterations,
        rank,
        hits,
        p50_us: pick(0.50),
        p95_us: pick(0.95),
        min_us: samples.first().copied().unwrap_or(0),
        max_us: samples.last().copied().unwrap_or(0),
    })
}

fn command_fts_build(arguments: &[String]) -> Result<(), String> {
    let (db, json) = simple_db_args(arguments, "fts-build")?;
    let conn = open_write_connection(&db).map_err(|error| error.to_string())?;
    let report = rebuild_fts(&conn).map_err(|error| error.to_string())?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
        );
    } else {
        println!(
            "built {} trigram rows in {} ms; index grew by {} bytes ({:.1} B/row); checkpoint {} ms",
            report.rows,
            report.build_ms,
            report.index_bytes,
            report.bytes_per_row,
            report.checkpoint_ms
        );
    }
    Ok(())
}

fn command_fts_status(arguments: &[String]) -> Result<(), String> {
    let (db, json) = simple_db_args(arguments, "fts-status")?;
    let conn = open_query_connection(&db).map_err(|error| error.to_string())?;
    let status = fts_status(&conn).map_err(|error| error.to_string())?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&status).map_err(|error| error.to_string())?
        );
    } else {
        println!(
            "table={} indexed_rows={} online_rows={} stale={} built_at={:?} build_ms={:?}",
            status.table_exists,
            status.indexed_rows,
            status.online_rows,
            status.stale,
            status.built_at,
            status.build_ms
        );
    }
    Ok(())
}

fn command_aggregate(arguments: &[String]) -> Result<(), String> {
    let (db, json) = simple_db_args(arguments, "aggregate")?;
    let conn = open_write_connection(&db).map_err(|error| error.to_string())?;
    let report = aggregate_dir_totals(&conn).map_err(|error| error.to_string())?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
        );
    } else {
        println!(
            "aggregated {} directories from {} files ({} online) in {} ms \
             (load {} ms, aggregate {} ms, write {} ms); {} bytes",
            report.directories,
            report.files_scanned,
            report.online_files,
            report.total_ms,
            report.load_ms,
            report.aggregate_ms,
            report.write_ms,
            report.bytes_indexed
        );
    }
    Ok(())
}

fn command_treemap(arguments: &[String]) -> Result<(), String> {
    let mut db: Option<PathBuf> = None;
    let mut path = None;
    let mut limit = 100usize;
    let mut json = false;
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        index += 1;
        match argument {
            "--db" => db = Some(PathBuf::from(take_value(arguments, &mut index, "--db")?)),
            "--path" => path = Some(take_value(arguments, &mut index, "--path")?),
            "--limit" => limit = parse_usize(take_value(arguments, &mut index, "--limit")?)?,
            "--json" => json = true,
            other => return Err(format!("unknown treemap option: {other}")),
        }
    }
    let db = db.ok_or_else(|| "--db is required".to_string())?;
    let conn = open_query_connection(&db).map_err(|error| error.to_string())?;
    let data = treemap_rows(&conn, path.as_deref(), limit).map_err(|error| error.to_string())?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&data).map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    println!(
        "{} ({} bytes, {} files, {} dirs) {} children in {} us",
        data.path,
        data.bytes,
        data.file_count,
        data.dir_count,
        data.children.len(),
        data.took_us
    );
    for child in data.children.iter().take(20) {
        println!(
            "{:>12}  {}{}",
            child.bytes,
            child.name,
            if child.is_dir { "/" } else { "" }
        );
    }
    Ok(())
}

fn command_children(arguments: &[String]) -> Result<(), String> {
    let mut db: Option<PathBuf> = None;
    let mut path: Option<String> = None;
    let mut limit = 200usize;
    let mut offset = 0usize;
    let mut sort = "size".to_string();
    let mut json = false;
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        index += 1;
        match argument {
            "--db" => db = Some(PathBuf::from(take_value(arguments, &mut index, "--db")?)),
            "--path" => path = Some(take_value(arguments, &mut index, "--path")?),
            "--limit" => limit = parse_usize(take_value(arguments, &mut index, "--limit")?)?,
            "--offset" => offset = parse_usize(take_value(arguments, &mut index, "--offset")?)?,
            "--sort" => sort = take_value(arguments, &mut index, "--sort")?,
            "--json" => json = true,
            other => return Err(format!("unknown children option: {other}")),
        }
    }
    let db = db.ok_or_else(|| "--db is required".to_string())?;
    let path = path.ok_or_else(|| "--path is required".to_string())?;
    let conn = open_query_connection(&db).map_err(|error| error.to_string())?;
    let page = child_rows(&conn, &path, limit, offset, &sort).map_err(|error| error.to_string())?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&page).map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    println!(
        "{}: {} children in {} us",
        page.path,
        page.children.len(),
        page.took_us
    );
    for child in page.children.iter().take(20) {
        println!(
            "{:>12}  {}{}",
            child.bytes,
            child.name,
            if child.is_dir { "/" } else { "" }
        );
    }
    Ok(())
}

fn command_dup(arguments: &[String]) -> Result<(), String> {
    let mut db: Option<PathBuf> = None;
    let mut partial_bytes = 4096usize;
    let mut limit = 100usize;
    let mut volume = None;
    let mut include_empty = false;
    let mut json = false;
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        index += 1;
        match argument {
            "--db" => db = Some(PathBuf::from(take_value(arguments, &mut index, "--db")?)),
            "--partial-bytes" => {
                partial_bytes = parse_usize(take_value(arguments, &mut index, "--partial-bytes")?)?
            }
            "--limit" => limit = parse_usize(take_value(arguments, &mut index, "--limit")?)?,
            "--volume" => volume = Some(take_value(arguments, &mut index, "--volume")?),
            "--include-empty" => include_empty = true,
            "--json" => json = true,
            other => return Err(format!("unknown dup option: {other}")),
        }
    }
    let db = db.ok_or_else(|| "--db is required".to_string())?;
    let conn = open_query_connection(&db).map_err(|error| error.to_string())?;
    let report = find_duplicates(
        &conn,
        &DuplicateOptions {
            partial_bytes,
            max_groups: limit,
            volume_id: volume,
            include_empty,
        },
    )
    .map_err(|error| error.to_string())?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    println!(
        "{} duplicate groups, {} redundant files, {} bytes reclaimable in {} ms",
        report.duplicate_groups, report.duplicate_files, report.reclaimable_bytes, report.took_ms
    );
    println!(
        "candidates {} in {} size groups; partial hashed {} ({} bytes, {} eliminated); \
         full hashed {} ({} bytes, {} eliminated); hardlink rows skipped {}; unreadable {}",
        report.candidate_files,
        report.candidate_size_groups,
        report.partial_hashed,
        report.partial_bytes,
        report.partial_eliminated,
        report.full_hashed,
        report.full_bytes,
        report.full_eliminated,
        report.skipped_hardlink_rows,
        report.skipped_unreadable_files
    );
    println!(
        "naive full-hash IO {} bytes vs actual {} bytes (saved {:.1}%)",
        report.naive_full_hash_bytes,
        report.partial_bytes + report.full_bytes,
        report.io_saved_ratio * 100.0
    );
    for group in report.groups.iter().take(5) {
        println!(
            "  {} bytes x{}: {}",
            group.size,
            group.file_count,
            group
                .files
                .first()
                .map(|file| file.path.as_str())
                .unwrap_or("")
        );
    }
    Ok(())
}

fn command_serve(arguments: &[String]) -> Result<(), String> {
    let mut db: Option<PathBuf> = None;
    let mut addr = "127.0.0.1:8787".to_string();
    let mut ui_dir = None;
    let mut bench_dir = None;
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        index += 1;
        match argument {
            "--db" => db = Some(PathBuf::from(take_value(arguments, &mut index, "--db")?)),
            "--addr" => addr = take_value(arguments, &mut index, "--addr")?,
            "--ui-dir" => {
                ui_dir = Some(PathBuf::from(take_value(
                    arguments, &mut index, "--ui-dir",
                )?))
            }
            "--bench-dir" => {
                bench_dir = Some(PathBuf::from(take_value(
                    arguments,
                    &mut index,
                    "--bench-dir",
                )?))
            }
            other => return Err(format!("unknown serve option: {other}")),
        }
    }
    let db = db.ok_or_else(|| "--db is required".to_string())?;
    serve(HttpOptions {
        db_path: db,
        addr,
        ui_dir,
        bench_dir,
    })
    .map_err(|error| error.to_string())
}

fn simple_db_args(arguments: &[String], command: &str) -> Result<(PathBuf, bool), String> {
    let mut db: Option<PathBuf> = None;
    let mut json = false;
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        index += 1;
        match argument {
            "--db" => db = Some(PathBuf::from(take_value(arguments, &mut index, "--db")?)),
            "--json" => json = true,
            other => return Err(format!("unknown {command} option: {other}")),
        }
    }
    Ok((db.ok_or_else(|| "--db is required".to_string())?, json))
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
