//! Cross-platform metadata helpers.
//!
//! Unix exposes `st_dev`/`st_ino`/`st_nlink` directly; Windows exposes the
//! volume serial number, file index and link count. Callers treat `None` as
//! "unknown" instead of silently comparing zeroes.

use std::fs::Metadata;

#[cfg(unix)]
pub fn device_id(metadata: &Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(metadata.dev())
}

#[cfg(windows)]
pub fn device_id(metadata: &Metadata) -> Option<u64> {
    use std::os::windows::fs::MetadataExt;
    metadata.volume_serial_number().map(u64::from)
}

#[cfg(not(any(unix, windows)))]
pub fn device_id(_metadata: &Metadata) -> Option<u64> {
    None
}

#[cfg(unix)]
pub fn inode(metadata: &Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(metadata.ino())
}

#[cfg(windows)]
pub fn inode(metadata: &Metadata) -> Option<u64> {
    use std::os::windows::fs::MetadataExt;
    metadata.file_index()
}

#[cfg(not(any(unix, windows)))]
pub fn inode(_metadata: &Metadata) -> Option<u64> {
    None
}

#[cfg(unix)]
pub fn nlink(metadata: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.nlink()
}

#[cfg(windows)]
pub fn nlink(metadata: &Metadata) -> u64 {
    use std::os::windows::fs::MetadataExt;
    u64::from(metadata.number_of_links().unwrap_or(1))
}

#[cfg(not(any(unix, windows)))]
pub fn nlink(_metadata: &Metadata) -> u64 {
    1
}
