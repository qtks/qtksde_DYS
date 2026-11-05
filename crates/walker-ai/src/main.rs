//! CLI for the AI layer. Every subcommand is read-only with respect to user
//! files; the only writes go to the index database (and, for suggestions, to
//! the `status` column).

use std::path::PathBuf;

use walker_ai::{db, embed, project, readonly, semantic, suggest, summary, vector};

fn usage() -> String {
    [
        "walker-ai <command>",
        "",
        "  projects      --db <path> [--volume <id>] [--mode full|incremental] [--json]",
        "  embed         --db <path> [--limit N] [--batch N] [--volume id] [--model-dir path] [--json]",
        "  semantic      --db <path> --q <text> [--k N] [--hybrid] [--model-dir path] [--json]",
        "  bench-vectors [--rows 100000] [--dim 512] [--shards 8] [--queries 50] [--k 10] [--json]",
        "  summarize     --db <path> --path <dir> [--limit N] [--json]",
        "  summarize-cloud --db <path> --path <dir> [--limit N] --endpoint <url> --model <name> --api-key-env <VAR> --consent",
        "  suggest       --db <path> [--min-cluster 3] [--limit 50] [--json]",
        "  suggest-list  --db <path> [--status pending|approved|rejected] [--json]",
        "  suggest-status --db <path> --id <id> --status approved|rejected|pending",
        "  audit         [--json]",
        "",
        "Model directory defaults to ../../.models/bge-small-zh-v1.5 relative to the crate.",
        "The model name and dimension are constants; no silent fallback is possible.",
    ]
    .join("\n")
}

fn flag(args: &[String], name: &str) -> Option<String> {
    let index = args.iter().position(|arg| arg == name)?;
    args.get(index + 1).cloned()
}

fn number(args: &[String], name: &str, default: usize) -> usize {
    flag(args, name)
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn has(args: &[String], name: &str) -> bool {
    args.iter().any(|arg| arg == name)
}

fn db_path(args: &[String]) -> PathBuf {
    flag(args, "--db")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("index.db"))
}

fn model_dir(args: &[String]) -> PathBuf {
    flag(args, "--model-dir")
        .map(PathBuf::from)
        .unwrap_or_else(embed::default_model_dir)
}

fn print_json<T: serde::Serialize>(value: &T) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_default()
    );
}

fn fail(message: impl std::fmt::Display) -> ! {
    eprintln!("walker-ai: {message}");
    std::process::exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first().cloned() else {
        eprintln!("{}", usage());
        std::process::exit(2);
    };
    let rest = &args[1..];
    let json = has(rest, "--json");

    match command.as_str() {
        "projects" => {
            let path = db_path(rest);
            let mode = match flag(rest, "--mode").as_deref() {
                Some("incremental") => project::RebuildMode::Incremental,
                _ => project::RebuildMode::Full,
            };
            let volume = flag(rest, "--volume");
            let mut conn = match db::open_rw(&path) {
                Ok(conn) => conn,
                Err(error) => fail(error),
            };
            match project::rebuild_projects(&mut conn, mode, volume.as_deref()) {
                Ok(report) => {
                    if json {
                        print_json(&report);
                    } else {
                        println!(
                            "mode={} roots={} assigned={} cleared={} suppressed_marker_dirs={} took={}ms",
                            report.mode, report.roots, report.assigned_rows, report.cleared_rows,
                            report.suppressed_marker_dirs, report.took_ms
                        );
                        for item in &report.projects {
                            println!(
                                "  {}  [{}]  {} files, {} bytes, markers={}",
                                item.root_path,
                                item.kind.as_str(),
                                item.file_count,
                                item.bytes,
                                item.markers.join(",")
                            );
                        }
                    }
                }
                Err(error) => fail(error),
            }
        }
        "embed" => {
            let path = db_path(rest);
            let mut embedder = match embed::Embedder::load(&model_dir(rest)) {
                Ok(embedder) => embedder,
                Err(error) => fail(error),
            };
            let mut conn = match db::open_rw(&path) {
                Ok(conn) => conn,
                Err(error) => fail(error),
            };
            let options = vector::EmbedOptions {
                limit: flag(rest, "--limit").and_then(|value| value.parse().ok()),
                batch: number(rest, "--batch", embed::BATCH_SIZE),
                volume: flag(rest, "--volume"),
            };
            match vector::embed_corpus(&mut conn, &mut embedder, &options) {
                Ok(report) => {
                    if json {
                        print_json(&report);
                    } else {
                        println!(
                            "embedded={} candidates={} unchanged={} content_same={} binary={} errors={}",
                            report.embedded, report.candidates, report.skipped_unchanged,
                            report.skipped_content_same, report.skipped_binary, report.errors
                        );
                        println!(
                            "vectors={} shards={} throughput={:.2} files/s took={}ms estimated_100k={:.0}s",
                            report.vectors_total, report.shards, report.files_per_second,
                            report.took_ms, report.estimated_100k_seconds
                        );
                    }
                }
                Err(error) => fail(error),
            }
        }
        "semantic" => {
            let path = db_path(rest);
            let query = match flag(rest, "--q") {
                Some(query) => query,
                None => fail("--q is required"),
            };
            let mut embedder = match embed::Embedder::load(&model_dir(rest)) {
                Ok(embedder) => embedder,
                Err(error) => fail(error),
            };
            let conn = match db::open_ro(&path) {
                Ok(conn) => conn,
                Err(error) => fail(error),
            };
            let options = semantic::SemanticOptions {
                k: number(rest, "--k", 10),
                hybrid: has(rest, "--hybrid"),
                shard: flag(rest, "--shard"),
            };
            match semantic::search(&conn, &mut embedder, &query, &options) {
                Ok(report) => {
                    if json {
                        print_json(&report);
                    } else {
                        println!(
                            "{} hits={} shards={} scanned={} embed={:.1}ms scan={:.1}ms took={}ms",
                            report.note,
                            report.hits.len(),
                            report.shards,
                            report.scanned_vectors,
                            report.embed_ms,
                            report.scan_ms,
                            report.took_ms
                        );
                        for hit in &report.hits {
                            println!(
                                "  {:.4}  sem={:.4} fts={:.4}  {}",
                                hit.score, hit.semantic, hit.fts, hit.path
                            );
                        }
                    }
                }
                Err(error) => fail(error),
            }
        }
        "bench-vectors" => {
            let report = vector::bench_vectors(
                number(rest, "--rows", 100_000),
                number(rest, "--dim", embed::EMBED_DIM),
                number(rest, "--shards", 8),
                number(rest, "--queries", 50),
                number(rest, "--k", 10),
            );
            if json {
                print_json(&report);
            } else {
                println!(
                    "rows={} dim={} shards={} queries={} p50={:.2}ms p95={:.2}ms worst={:.2}ms avg={:.2}ms build={}ms",
                    report.rows, report.dim, report.shards, report.queries,
                    report.p50_ms, report.p95_ms, report.worst_ms, report.avg_ms, report.build_ms
                );
            }
        }
        "summarize" => {
            let path = db_path(rest);
            let target = match flag(rest, "--path") {
                Some(path) => path,
                None => fail("--path is required"),
            };
            let conn = match db::open_ro(&path) {
                Ok(conn) => conn,
                Err(error) => fail(error),
            };
            let report = match summary::summarize_dir(&conn, &target, number(rest, "--limit", 10)) {
                Ok(report) => report,
                Err(error) => fail(error),
            };
            if json {
                print_json(&report);
            } else {
                println!("{}", report.digest);
                println!("generator={} note={}", report.generator, report.note);
                for file in &report.largest_files {
                    println!("  {}  {} bytes", file.path, file.size);
                }
            }
        }
        "summarize-cloud" => {
            let path = db_path(rest);
            let target = match flag(rest, "--path") {
                Some(path) => path,
                None => fail("--path is required"),
            };
            let conn = match db::open_ro(&path) {
                Ok(conn) => conn,
                Err(error) => fail(error),
            };
            let scope = match summary::cloud_scope(&conn, &target, number(rest, "--limit", 20)) {
                Ok(scope) => scope,
                Err(error) => fail(error),
            };
            // The scope is printed before anything is sent, always.
            eprintln!(
                "UPLOAD SCOPE: {} files, {} bytes, payload {} chars\n{}",
                scope.files, scope.body_bytes, scope.body_chars, scope.payload_preview
            );
            let endpoint = flag(rest, "--endpoint").unwrap_or_default();
            let model = flag(rest, "--model").unwrap_or_else(|| "gpt-4o-mini".to_string());
            let key_env =
                flag(rest, "--api-key-env").unwrap_or_else(|| "OPENAI_API_KEY".to_string());
            let api_key = std::env::var(&key_env).unwrap_or_default();
            let config = summary::CloudConfig {
                endpoint,
                model,
                api_key,
            };
            match summary::cloud_summarize(&scope, &config, has(rest, "--consent")) {
                Ok(text) => println!("{text}"),
                Err(error) => fail(error),
            }
        }
        "suggest" => {
            let path = db_path(rest);
            let mut conn = match db::open_rw(&path) {
                Ok(conn) => conn,
                Err(error) => fail(error),
            };
            match suggest::generate(
                &mut conn,
                number(rest, "--min-cluster", 3),
                number(rest, "--limit", 50),
            ) {
                Ok(report) => {
                    if json {
                        print_json(&report);
                    } else {
                        println!(
                            "generated={} kept_existing={} min_cluster={} kinds={}",
                            report.generated,
                            report.kept,
                            report.min_cluster,
                            report.kinds.join(",")
                        );
                    }
                }
                Err(error) => fail(error),
            }
        }
        "suggest-list" => {
            let path = db_path(rest);
            let conn = match db::open_ro(&path) {
                Ok(conn) => conn,
                Err(error) => fail(error),
            };
            match suggest::list(&conn, flag(rest, "--status").as_deref()) {
                Ok(items) => {
                    if json {
                        print_json(&items);
                    } else {
                        for item in &items {
                            println!(
                                "{} [{}] {} — {}",
                                item.id, item.status, item.kind, item.evidence
                            );
                        }
                    }
                }
                Err(error) => fail(error),
            }
        }
        "suggest-status" => {
            let path = db_path(rest);
            let id = match flag(rest, "--id") {
                Some(id) => id,
                None => fail("--id is required"),
            };
            let status = match flag(rest, "--status") {
                Some(status) => status,
                None => fail("--status is required"),
            };
            let mut conn = match db::open_rw(&path) {
                Ok(conn) => conn,
                Err(error) => fail(error),
            };
            match suggest::set_status(&mut conn, &id, &status) {
                Ok(true) => println!("{id} -> {status} (status column only; no file was touched)"),
                Ok(false) => fail(format!("suggestion {id} not found")),
                Err(error) => fail(error),
            }
        }
        "audit" => {
            let report = match readonly::audit() {
                Ok(report) => report,
                Err(error) => fail(error),
            };
            if json {
                print_json(&report);
            } else {
                println!(
                    "files={} lines={} test_lines_excluded={} query_only={} ok={}",
                    report.files_scanned,
                    report.lines_scanned,
                    report.test_lines_excluded,
                    report.query_only,
                    report.ok
                );
                for violation in &report.violations {
                    println!(
                        "  {}:{} {}",
                        violation.file, violation.line, violation.token
                    );
                }
            }
        }
        "help" | "--help" | "-h" => println!("{}", usage()),
        other => fail(format!("unknown command {other}\n\n{}", usage())),
    }
}
