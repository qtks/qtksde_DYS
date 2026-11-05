use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use rusqlite::params;
use serde_json::json;

use crate::dedupe::{find_duplicates, DuplicateOptions};
use crate::search::{search, PrefixMode, SearchKind, SearchOptions};
use crate::treemap::{child_rows, treemap_rows};
use crate::{open_query_connection, Result};

#[derive(Debug, Clone)]
pub struct HttpOptions {
    pub db_path: PathBuf,
    pub addr: String,
    /// Built frontend (`ui/dist`). When present, non-`/api` paths are served
    /// from it, which is what Tauri loads in production.
    pub ui_dir: Option<PathBuf>,
    /// Directory where browser benchmark payloads are written.
    pub bench_dir: Option<PathBuf>,
}

pub fn serve(options: HttpOptions) -> Result<()> {
    let listener = TcpListener::bind(&options.addr)
        .map_err(|error| crate::IndexError::Config(format!("bind {}: {error}", options.addr)))?;
    // A CLI scan does not maintain `dir_totals`, so the first window after a
    // scan would show 0 B and an empty treemap. Build it once here; the scan
    // path itself stays untouched. The listener is already bound, so the
    // shell's readiness probe succeeds while this runs.
    if let Ok(conn) = crate::open_write_connection(&options.db_path) {
        match crate::treemap::ensure_dir_totals(&conn) {
            Ok(Some(report)) => println!(
                "walker-query: built dir totals for {} directories ({} files, {} bytes) in {} ms",
                report.directories, report.online_files, report.bytes_indexed, report.total_ms
            ),
            Ok(None) => {}
            Err(error) => eprintln!("walker-query: could not build dir totals: {error}"),
        }
        match crate::fts::ensure_fts(&conn) {
            Ok(Some(report)) => println!(
                "walker-query: built FTS index for {} rows in {} ms",
                report.rows, report.build_ms
            ),
            Ok(None) => {}
            Err(error) => eprintln!("walker-query: could not build FTS index: {error}"),
        }
    }
    let options = Arc::new(options);
    println!("walker-query listening on http://{}", options.addr);
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("accept error: {error}");
                continue;
            }
        };
        let options = Arc::clone(&options);
        std::thread::spawn(move || {
            if let Err(error) = handle(stream, &options) {
                eprintln!("request error: {error}");
            }
        });
    }
    Ok(())
}

fn handle(mut stream: TcpStream, options: &HttpOptions) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let target = parts.next().unwrap_or("/").to_string();

    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        if header.trim().is_empty() {
            break;
        }
        if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }

    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), parse_query(query)),
        None => (target.clone(), HashMap::new()),
    };

    let response = if method == "OPTIONS" {
        empty_response(204)
    } else if path.starts_with("/api/") {
        match route(&path, &query, &method, &body, options) {
            Ok(response) => response,
            Err(error) => json_response(500, &json!({ "error": error.to_string() })),
        }
    } else {
        static_response(&path, options)
    };

    stream.write_all(&response)?;
    stream.flush()
}

fn route(
    path: &str,
    query: &HashMap<String, String>,
    method: &str,
    body: &[u8],
    options: &HttpOptions,
) -> Result<Vec<u8>> {
    let started = Instant::now();
    match path {
        "/api/health" => Ok(json_response(
            200,
            &json!({
                "ok": true,
                "db": options.db_path.to_string_lossy(),
                "took_ms": started.elapsed().as_millis() as u64,
            }),
        )),
        "/api/volumes" => {
            let conn = open_query_connection(&options.db_path)?;
            let payload = volumes_payload(&conn)?;
            Ok(json_response(200, &payload))
        }
        "/api/search" => {
            let kind = query.get("kind").map(|v| v.as_str()).unwrap_or("exact");
            let kind = SearchKind::parse(kind)
                .ok_or_else(|| crate::IndexError::Config(format!("unknown kind: {kind}")))?;
            let text = query.get("q").cloned().unwrap_or_default();
            let prefix_mode = query
                .get("prefix_mode")
                .map(|value| value.as_str())
                .unwrap_or("range");
            let prefix_mode = PrefixMode::parse(prefix_mode).ok_or_else(|| {
                crate::IndexError::Config(format!("unknown prefix_mode: {prefix_mode}"))
            })?;
            let conn = open_query_connection(&options.db_path)?;
            let result = search(
                &conn,
                &SearchOptions {
                    kind,
                    query: text,
                    limit: parse_number(query, "limit", 200),
                    prefix_mode,
                    volume_id: query.get("volume").cloned(),
                    offset: parse_number(query, "offset", 0),
                    rank: query
                        .get("rank")
                        .map(|value| value == "1" || value == "true")
                        .unwrap_or(false),
                },
            )?;
            Ok(json_response(200, &result))
        }
        "/api/children" => {
            let path = query.get("path").cloned().unwrap_or_default();
            if path.is_empty() {
                return Err(crate::IndexError::Config("path is required".to_string()));
            }
            let conn = open_query_connection(&options.db_path)?;
            let page = child_rows(
                &conn,
                &path,
                parse_number(query, "limit", 500),
                parse_number(query, "offset", 0),
                query.get("sort").map(|v| v.as_str()).unwrap_or("size"),
            )?;
            Ok(json_response(200, &page))
        }
        "/api/treemap" => {
            let conn = open_query_connection(&options.db_path)?;
            let data = treemap_rows(
                &conn,
                query.get("path").map(|value| value.as_str()),
                parse_number(query, "limit", 200),
            )?;
            Ok(json_response(200, &data))
        }
        "/api/duplicates" => {
            let conn = open_query_connection(&options.db_path)?;
            let report = find_duplicates(
                &conn,
                &DuplicateOptions {
                    partial_bytes: parse_number(query, "partial_bytes", 4096),
                    max_groups: parse_number(query, "limit", 100),
                    volume_id: query.get("volume").cloned(),
                    include_empty: query
                        .get("include_empty")
                        .map(|value| value == "1" || value == "true")
                        .unwrap_or(false),
                },
            )?;
            Ok(json_response(200, &report))
        }
        "/api/files" => {
            let conn = open_query_connection(&options.db_path)?;
            let limit = parse_number(query, "limit", 1000);
            let offset = parse_number(query, "offset", 0);
            let volume = query.get("volume").cloned().unwrap_or_default();
            let body = bulk_rows(&conn, limit, offset, &volume)?;
            Ok(raw_response(200, "application/json; charset=utf-8", body))
        }
        "/api/stats" => {
            let conn = open_query_connection(&options.db_path)?;
            let files: i64 = conn.query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))?;
            let dirs: i64 =
                conn.query_row("SELECT COUNT(*) FROM files WHERE is_dir=1", [], |row| {
                    row.get(0)
                })?;
            let bytes: i64 = conn.query_row(
                "SELECT COALESCE(SUM(size),0) FROM files WHERE is_dir=0 AND status='online'",
                [],
                |row| row.get(0),
            )?;
            Ok(json_response(
                200,
                &json!({
                    "rows": files,
                    "dirs": dirs,
                    "bytes": bytes,
                }),
            ))
        }
        "/api/bench-report" => {
            if method != "POST" {
                return Err(crate::IndexError::Config(
                    "POST is required for /api/bench-report".to_string(),
                ));
            }
            let directory = options
                .bench_dir
                .clone()
                .ok_or_else(|| crate::IndexError::Config("bench dir not configured".to_string()))?;
            fs::create_dir_all(&directory)?;
            let report: serde_json::Value = serde_json::from_slice(body)?;
            let name = report
                .get("name")
                .and_then(|value| value.as_str())
                .unwrap_or("browser-bench")
                .chars()
                .map(|ch| {
                    if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                        ch
                    } else {
                        '-'
                    }
                })
                .collect::<String>();
            let file = directory.join(format!("{name}.json"));
            fs::write(&file, serde_json::to_vec_pretty(&report)?)?;
            Ok(json_response(
                200,
                &json!({ "saved": file.to_string_lossy() }),
            ))
        }
        other => Err(crate::IndexError::Config(format!(
            "unknown endpoint: {other}"
        ))),
    }
}

fn volumes_payload(conn: &rusqlite::Connection) -> Result<serde_json::Value> {
    let mut statement = conn.prepare(
        "SELECT v.id, v.name, v.mount_path, v.status,
                COALESCE((SELECT t.bytes FROM files f LEFT JOIN dir_totals t ON t.dir_id = f.id
                          WHERE f.volume_id = v.id AND f.path = v.mount_path AND f.is_dir = 1), 0),
                COALESCE((SELECT t.file_count FROM files f LEFT JOIN dir_totals t ON t.dir_id = f.id
                          WHERE f.volume_id = v.id AND f.path = v.mount_path AND f.is_dir = 1), 0),
                COALESCE((SELECT t.dir_count FROM files f LEFT JOIN dir_totals t ON t.dir_id = f.id
                          WHERE f.volume_id = v.id AND f.path = v.mount_path AND f.is_dir = 1), 0)
         FROM volumes v
         ORDER BY v.name",
    )?;
    let mapped = statement.query_map([], |row| {
        Ok(json!({
            "id": row.get::<_, String>(0)?,
            "name": row.get::<_, String>(1)?,
            "mount_path": row.get::<_, String>(2)?,
            "status": row.get::<_, String>(3)?,
            "bytes": row.get::<_, i64>(4)?.max(0),
            "file_count": row.get::<_, i64>(5)?.max(0),
            "dir_count": row.get::<_, i64>(6)?.max(0),
        }))
    })?;
    Ok(json!({
        "volumes": mapped.collect::<std::result::Result<Vec<_>, _>>()?,
    }))
}

/// Compact rows `[path, name, size, is_dir, mtime]` used by the virtual
/// scroll benchmark. Written by hand because serde_json::Value per row would
/// allocate several objects for 500k rows.
fn bulk_rows(
    conn: &rusqlite::Connection,
    limit: usize,
    offset: usize,
    volume: &str,
) -> Result<Vec<u8>> {
    let started = Instant::now();
    let limit = limit.clamp(1, 1_000_000);
    let mut statement = conn.prepare(
        "SELECT path, name, size, is_dir, mtime FROM files
         WHERE is_dir = 0 AND status = 'online'
           AND (?1 = '' OR volume_id = ?1)
         ORDER BY path
         LIMIT ?2 OFFSET ?3",
    )?;
    let mut rows = statement.query(params![volume, limit as i64, offset as i64])?;
    let mut out = String::with_capacity(limit * 80);
    out.push_str("{\"rows\":[");
    let mut count = 0usize;
    while let Some(row) = rows.next()? {
        if count > 0 {
            out.push(',');
        }
        let path: String = row.get(0)?;
        let name: String = row.get(1)?;
        let size: i64 = row.get(2)?;
        let is_dir: i64 = row.get(3)?;
        let mtime: i64 = row.get(4)?;
        out.push('[');
        out.push_str(&serde_json::to_string(&path)?);
        out.push(',');
        out.push_str(&serde_json::to_string(&name)?);
        out.push(',');
        out.push_str(&size.to_string());
        out.push(',');
        out.push_str(&is_dir.to_string());
        out.push(',');
        out.push_str(&mtime.to_string());
        out.push(']');
        count += 1;
    }
    out.push_str("],\"count\":");
    out.push_str(&count.to_string());
    out.push_str(",\"took_ms\":");
    out.push_str(&started.elapsed().as_millis().to_string());
    out.push('}');
    Ok(out.into_bytes())
}

fn parse_number(query: &HashMap<String, String>, key: &str, default: usize) -> usize {
    query
        .get(key)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
}

fn parse_query(raw: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in raw.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = match pair.split_once('=') {
            Some((key, value)) => (key, value),
            None => (pair, ""),
        };
        map.insert(percent_decode(key), percent_decode(value));
    }
    map
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn static_response(path: &str, options: &HttpOptions) -> Vec<u8> {
    let Some(directory) = &options.ui_dir else {
        return json_response(404, &json!({ "error": "UI bundle not configured" }));
    };
    let relative = path.trim_start_matches('/');
    let relative = if relative.is_empty() {
        "index.html"
    } else {
        relative
    };
    if relative.contains("..") {
        return json_response(400, &json!({ "error": "invalid path" }));
    }
    let file = directory.join(relative);
    let file = if file.is_file() {
        file
    } else {
        directory.join("index.html")
    };
    match fs::read(&file) {
        Ok(bytes) => raw_response(200, content_type(&file), bytes),
        Err(_) => json_response(404, &json!({ "error": "not found" })),
    }
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("json") => "application/json; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn empty_response(status: u16) -> Vec<u8> {
    let mut out = Vec::new();
    let _ = write!(
        out,
        "HTTP/1.1 {status} {}\r\nAccess-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
         Access-Control-Allow-Headers: Content-Type\r\n\
         Content-Length: 0\r\nConnection: close\r\n\r\n",
        status_text(status)
    );
    out
}

fn json_response<T: serde::Serialize>(status: u16, value: &T) -> Vec<u8> {
    let body = serde_json::to_vec(value).unwrap_or_default();
    raw_response(status, "application/json; charset=utf-8", body)
}

fn raw_response(status: u16, content_type: &str, body: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 256);
    let _ = write!(
        out,
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\n\
         Content-Length: {}\r\nAccess-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
         Access-Control-Allow-Headers: Content-Type\r\n\
         Connection: close\r\n\r\n",
        status_text(status),
        body.len()
    );
    out.extend_from_slice(&body);
    out
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "OK",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A first launch has an empty index and no aggregate run yet. `/api/volumes`
    /// joins `dir_totals`, so the table must exist as soon as the schema is
    /// created - otherwise the UI opens on "no such table: dir_totals".
    #[test]
    fn volumes_payload_works_on_a_fresh_index() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("index.db");
        walker_index::IndexDb::open(&path).expect("schema");
        let conn = open_query_connection(&path).expect("open query connection");
        let payload = volumes_payload(&conn).expect("volumes payload");
        assert_eq!(
            payload["volumes"].as_array().expect("volumes array").len(),
            0
        );
    }

    /// The v0.1.0 database is an M1 schema without `dir_totals`; opening it
    /// must migrate in place so `/api/volumes` stops answering 500.
    #[test]
    fn volumes_payload_works_after_migrating_an_m1_database() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("index.db");
        {
            let conn = rusqlite::Connection::open(&path).expect("fixture");
            conn.execute_batch(walker_index::SCHEMA_SQL)
                .expect("m1 schema");
            conn.execute(
                "INSERT INTO meta(key, value) VALUES ('schema_version', 'panopticon.m1.index.v1')",
                [],
            )
            .expect("meta row");
        }
        walker_index::IndexDb::open(&path).expect("migrate m1 database");
        let conn = open_query_connection(&path).expect("open query connection");
        let payload = volumes_payload(&conn).expect("volumes payload");
        assert_eq!(
            payload["volumes"].as_array().expect("volumes array").len(),
            0
        );
    }
}
