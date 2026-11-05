//! Cross-platform metadata helpers.
//!
//! Unix exposes `st_dev`/`st_ino`/`st_nlink` directly. The equivalent Windows
//! accessors (`volume_serial_number`, `file_index`, `number_of_links`) are
//! still behind the unstable `windows_by_handle` feature, so Windows reports
//! "unknown" here: the volume probe then treats a present root as mounted and
//! hardlink aggregation stays off. Nothing silently compares zeroes.

use std::fs::Metadata;

#[cfg(unix)]
pub fn device_id(metadata: &Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(metadata.dev())
}

#[cfg(windows)]
pub fn device_id(_metadata: &Metadata) -> Option<u64> {
    None
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
pub fn inode(_metadata: &Metadata) -> Option<u64> {
    None
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
pub fn nlink(_metadata: &Metadata) -> u64 {
    1
}

#[cfg(not(any(unix, windows)))]
pub fn nlink(_metadata: &Metadata) -> u64 {
    1
}
