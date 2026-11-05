//! Tauri 2 shell for Panopticon.
//!
//! The window loads the same bundle the embedded `walker-query` server serves,
//! so `/api/*` stays same-origin and there is only one data path.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tauri::Manager;

const APP_DATA_DIR: &str = "panopticon";

/// Fixed default location: `$XDG_DATA_HOME/panopticon/index.db`, falling back
/// to `~/.local/share/panopticon/index.db`. A relative `index.db` used to
/// depend on the working directory, so the same user could open different
/// (empty) databases from a launcher and a terminal.
fn default_db_path(xdg: Option<PathBuf>, home: Option<PathBuf>, cwd: &Path) -> PathBuf {
    let base = match xdg {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => match home {
            Some(home) => home.join(".local").join("share"),
            None => cwd.to_path_buf(),
        },
    };
    base.join(APP_DATA_DIR).join("index.db")
}

/// Where the pre-XDG build may have left an index: the launch directory, plus
/// `$HOME` because desktop launchers commonly run the binary from there.
fn legacy_candidates(cwd: &Path, home: Option<&Path>, target: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![cwd.join("index.db")];
    if let Some(home) = home {
        candidates.push(home.join("index.db"));
    }
    candidates.retain(|path| path != target && path.is_file());
    candidates
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = std::fs::metadata(path) {
        let mut permissions = metadata.permissions();
        permissions.set_mode(mode);
        let _ = std::fs::set_permissions(path, permissions);
    }
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}

/// Create (if needed) and tighten the app's own data directory to 0700.
fn prepare_private_dir(dir: &Path) {
    if let Err(error) = std::fs::create_dir_all(dir) {
        eprintln!("could not create {}: {error}", dir.display());
        return;
    }
    set_mode(dir, 0o700);
}

fn copy_legacy_index(legacy: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::copy(legacy, target)?;
    // Un-checkpointed writes live in the -wal sidecar; copy it too.
    let legacy_wal = with_suffix(legacy, "-wal");
    if legacy_wal.is_file() {
        let target_wal = with_suffix(target, "-wal");
        std::fs::copy(&legacy_wal, &target_wal)?;
        set_mode(&target_wal, 0o600);
    }
    set_mode(target, 0o600);
    Ok(())
}

/// Move the default database to the XDG location the first time the new build
/// runs. The original file is copied, never removed.
fn prepare_default_db(target: &Path, cwd: &Path, home: Option<&Path>) {
    if let Some(parent) = target.parent() {
        prepare_private_dir(parent);
    }
    if target.exists() {
        set_mode(target, 0o600);
        return;
    }
    for legacy in legacy_candidates(cwd, home, target) {
        match copy_legacy_index(&legacy, target) {
            Ok(()) => {
                eprintln!(
                    "index: copied existing database {} -> {} (original kept)",
                    legacy.display(),
                    target.display()
                );
                return;
            }
            Err(error) => eprintln!(
                "index: could not copy {} -> {}: {error}",
                legacy.display(),
                target.display()
            ),
        }
    }
}

fn db_path_from(
    override_path: Option<PathBuf>,
    xdg: Option<PathBuf>,
    home: Option<PathBuf>,
    cwd: &Path,
) -> PathBuf {
    override_path.unwrap_or_else(|| default_db_path(xdg, home, cwd))
}

fn backend_db() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let override_path = std::env::var_os("PANOPTICON_DB").map(PathBuf::from);
    let target = db_path_from(
        override_path.clone(),
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
        home.clone(),
        &cwd,
    );
    if override_path.is_some() {
        if let Some(parent) = target.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        return target;
    }
    prepare_default_db(&target, &cwd, home.as_deref());
    target
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

/// Open the index once at startup. This is deliberately unconditional: the
/// schema migrations in `IndexDb::open` are what bring an existing M1 database
/// (created by v0.1.0) up to the tables the UI queries. Skipping the open when
/// the file merely exists is how the old build left users on
/// "no such table: dir_totals".
fn ensure_index(db: &Path) {
    match walker_index::IndexDb::open(db) {
        Ok(_) => set_mode(db, 0o600),
        Err(error) => {
            set_mode(db, 0o600);
            eprintln!(
                "could not migrate {}: {error}; index queries will fail until this is fixed",
                db.display()
            );
        }
    }
}

/// Loopback traffic must bypass any system HTTP proxy.
///
/// The window loads the UI from `http://127.0.0.1:<port>/`. WebKitGTK takes its
/// proxy configuration from GNOME (and WebView2 from WinINet); when a proxy is
/// configured, those loopback requests can be sent through it, and a proxy that
/// is down or refuses loopback leaves the window completely white. Listing
/// loopback in `no_proxy` makes GTK/WebKit talk to the local server directly.
fn bypass_loopback_proxy() {
    const LOOPBACK: [&str; 3] = ["127.0.0.1", "localhost", "::1"];
    for key in ["no_proxy", "NO_PROXY"] {
        let existing = std::env::var(key).unwrap_or_default();
        let mut parts: Vec<String> = existing
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(str::to_string)
            .collect();
        for host in LOOPBACK {
            if !parts.iter().any(|part| part == host) {
                parts.push(host.to_string());
            }
        }
        std::env::set_var(key, parts.join(","));
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    bypass_loopback_proxy();
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("panopticon-lib-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    #[test]
    fn xdg_data_home_wins_over_the_working_directory() {
        let path = default_db_path(
            Some(PathBuf::from("/data")),
            Some(PathBuf::from("/home/user")),
            Path::new("/work"),
        );
        assert_eq!(path, PathBuf::from("/data/panopticon/index.db"));
    }

    #[test]
    fn falls_back_to_home_local_share() {
        let path = default_db_path(None, Some(PathBuf::from("/home/user")), Path::new("/work"));
        assert_eq!(
            path,
            PathBuf::from("/home/user/.local/share/panopticon/index.db")
        );
    }

    #[test]
    fn empty_xdg_is_treated_as_unset() {
        let path = default_db_path(
            Some(PathBuf::new()),
            Some(PathBuf::from("/home/user")),
            Path::new("/work"),
        );
        assert_eq!(
            path,
            PathBuf::from("/home/user/.local/share/panopticon/index.db")
        );
    }

    #[test]
    fn panopticon_db_override_wins() {
        let path = db_path_from(
            Some(PathBuf::from("/custom/index.db")),
            Some(PathBuf::from("/data")),
            Some(PathBuf::from("/home/user")),
            Path::new("/work"),
        );
        assert_eq!(path, PathBuf::from("/custom/index.db"));
    }

    #[test]
    fn legacy_database_is_copied_and_the_original_kept() {
        let root = scratch("legacy");
        let cwd = root.join("cwd");
        let home = root.join("home");
        fs::create_dir_all(&cwd).expect("cwd");
        fs::create_dir_all(&home).expect("home");
        let legacy = cwd.join("index.db");
        fs::write(&legacy, b"legacy-bytes").expect("legacy db");

        let target = default_db_path(None, Some(home.clone()), &cwd);
        prepare_default_db(&target, &cwd, Some(&home));

        assert_eq!(fs::read(&target).expect("target"), b"legacy-bytes");
        assert!(legacy.is_file(), "the original database must be kept");
        #[cfg(unix)]
        {
            assert_eq!(mode(target.parent().expect("parent")), 0o700);
            assert_eq!(mode(&target), 0o600);
        }
    }

    #[test]
    fn migrated_legacy_database_opens_at_the_current_schema() {
        let root = scratch("schema");
        let cwd = root.join("cwd");
        let home = root.join("home");
        fs::create_dir_all(&cwd).expect("cwd");
        fs::create_dir_all(&home).expect("home");

        // Simulate the v0.1.0 database: current schema minus the migrations.
        let legacy = cwd.join("index.db");
        let db = walker_index::IndexDb::open(&legacy).expect("create legacy db");
        db.connection()
            .execute_batch("DROP TABLE dir_totals; PRAGMA user_version=0;")
            .expect("downgrade fixture");
        drop(db);

        let target = default_db_path(None, Some(home.clone()), &cwd);
        prepare_default_db(&target, &cwd, Some(&home));

        let migrated = walker_index::IndexDb::open(&target).expect("migrate copied database");
        let version: i64 = migrated
            .connection()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("user_version");
        assert_eq!(version as u32, walker_index::SCHEMA_VERSION);
        let dir_totals: i64 = migrated
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='dir_totals'",
                [],
                |row| row.get(0),
            )
            .expect("sqlite_master");
        assert_eq!(dir_totals, 1);
    }
}
