#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("aborted after {batches} batch(es); checkpoint retained for resume")]
    Aborted { batches: u64 },
    #[error("injected failure after {rows} row(s); batch rolled back")]
    InjectedFailure { rows: u64 },
    #[error("volume not found: {0}")]
    VolumeNotFound(String),
}
