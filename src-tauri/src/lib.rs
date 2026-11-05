//! Tauri 2 shell for Panopticon.
//!
//! The desktop window is not a second UI: it loads the same bundle the
//! `walker-query serve` process serves, so the frontend keeps hitting `/api/*`
//! on the same origin and there is no duplicated data path.

use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const ADDR: &str = "127.0.0.1:8787";

fn backend_db() -> PathBuf {
    std::env::var_os("PANOPTICON_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("index.db"))
}

fn backend_ui() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ui/dist")
}

fn wait_for_backend() {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if TcpStream::connect(ADDR).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let options = walker_query::HttpOptions {
        db_path: backend_db(),
        addr: ADDR.to_string(),
        ui_dir: Some(backend_ui()),
        bench_dir: None,
    };
    std::thread::spawn(move || {
        if let Err(error) = walker_query::serve(options) {
            eprintln!("walker-query backend failed: {error}");
        }
    });
    wait_for_backend();

    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
