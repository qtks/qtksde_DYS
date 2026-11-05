//! Tauri 2 shell for Panopticon.
//!
//! The window loads the same bundle the embedded `walker-query` server serves,
//! so `/api/*` stays same-origin and there is only one data path.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tauri::Manager;

fn backend_db() -> PathBuf {
    std::env::var_os("PANOPTICON_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("index.db"))
}

/// Prefer 8787, but if another process owns it (a CLI `walker-query serve`,
/// for instance) fall back to an ephemeral port instead of failing to start.
fn backend_addr() -> String {
    if TcpListener::bind("127.0.0.1:8787").is_ok() {
        return "127.0.0.1:8787".to_string();
    }
    TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| "127.0.0.1:8788".to_string())
}

fn wait_for(addr: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(addr).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn ui_dir(app: &tauri::App) -> PathBuf {
    app.path()
        .resource_dir()
        .ok()
        .map(|dir| dir.join("ui"))
        .filter(|dir| dir.join("index.html").is_file())
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ui/dist"))
}

/// First launch: create the empty index schema so the UI can say "no volumes
/// indexed yet" instead of failing on a missing database file.
fn ensure_index(db: &Path) {
    if db.is_file() {
        return;
    }
    if let Err(error) = walker_index::IndexDb::open(db) {
        eprintln!("could not create {}: {error}", db.display());
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let db = backend_db();
            ensure_index(&db);
            let addr = backend_addr();
            let ui = ui_dir(app);
            let server_addr = addr.clone();
            let server_ui = ui.clone();
            // Diagnostics hooks: `PANOPTICON_BENCH_DIR` makes the backend accept
            // `/api/bench-report` posts, and `PANOPTICON_UI_QUERY` (e.g.
            // `bench=scroll&rows=500000`) loads the window straight into a
            // benchmark route. Both are optional and ignored by normal runs.
            let bench_dir = std::env::var_os("PANOPTICON_BENCH_DIR").map(PathBuf::from);
            std::thread::spawn(move || {
                let options = walker_query::HttpOptions {
                    db_path: db,
                    addr: server_addr.clone(),
                    ui_dir: Some(server_ui),
                    bench_dir,
                };
                if let Err(error) = walker_query::serve(options) {
                    eprintln!("walker-query backend failed on {server_addr}: {error}");
                }
            });
            wait_for(&addr);

            let query = std::env::var("PANOPTICON_UI_QUERY").unwrap_or_default();
            let url = if query.is_empty() {
                format!("http://{addr}/")
            } else {
                format!("http://{addr}/?{query}")
            };
            tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::External(url.parse()?),
            )
            .title("Panopticon")
            .inner_size(1280.0, 840.0)
            .min_inner_size(900.0, 600.0)
            .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
