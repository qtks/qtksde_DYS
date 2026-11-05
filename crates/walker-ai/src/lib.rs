//! Panopticon M3: local AI layer.
//!
//! Everything in this crate treats the user's file system as read-only. The
//! only writes go to auxiliary SQLite tables inside the index database
//! (`projects`, `vectors`, `vector_shards`, `archive_suggestions`).

pub mod db;
pub mod embed;
pub mod project;
pub mod readonly;
pub mod semantic;
pub mod suggest;
pub mod summary;
pub mod vector;

pub use walker_index::IndexError;

pub type Result<T> = std::result::Result<T, IndexError>;
