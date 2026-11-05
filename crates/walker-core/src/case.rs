use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::model::{CaseProbe, CaseSensitivity};

/// Detect case sensitivity without writing to the scanned directory.
///
/// First we take an existing entry with an ASCII letter, flip its case and
/// compare device/inode. If that is inconclusive we fall back to the filesystem
/// type reported by `/proc/mounts`.
pub fn probe(root: &Path) -> CaseProbe {
    if let Some(probe) = probe_by_case_flip(root) {
        return probe;
    }
    if let Some(probe) = probe_by_mount_table(root) {
        return probe;
    }
    CaseProbe::unknown("unavailable")
}

fn probe_by_case_flip(root: &Path) -> Option<CaseProbe> {
    let read_dir = fs::read_dir(root).ok()?;
    let mut checked = 0usize;
    for entry in read_dir.flatten() {
        if checked >= 64 {
            break;
        }
        let name = entry.file_name();
        let Some(name_text) = name.to_str() else {
            continue;
        };
        let Some(flipped) = flip_first_ascii_letter(name_text) else {
            continue;
        };
        checked += 1;
        let original = entry.path();
        let altered = root.join(&flipped);
        let original_meta = match fs::symlink_metadata(&original) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        match fs::symlink_metadata(&altered) {
            Ok(altered_meta) => {
                let same = original_meta.dev() == altered_meta.dev()
                    && original_meta.ino() == altered_meta.ino();
                let sensitivity = if same {
                    CaseSensitivity::Insensitive
                } else {
                    CaseSensitivity::Sensitive
                };
                return Some(CaseProbe {
                    sensitivity,
                    method: "case_flip_probe".to_string(),
                    evidence: Some(format!("{name_text} -> {flipped}")),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Some(CaseProbe {
                    sensitivity: CaseSensitivity::Sensitive,
                    method: "case_flip_probe".to_string(),
                    evidence: Some(format!("{name_text} -> {flipped} (not found)")),
                });
            }
            Err(_) => continue,
        }
    }
    None
}

fn probe_by_mount_table(root: &Path) -> Option<CaseProbe> {
    let target = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mounts = fs::read_to_string("/proc/mounts").ok()?;
    let mut best: Option<(usize, String, String)> = None;
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let Some(mount_point) = fields.nth(1) else {
            continue;
        };
        let Some(fs_type) = fields.next() else {
            continue;
        };
        let mount_path = PathBuf::from(unescape_mount_field(mount_point));
        if !target.starts_with(&mount_path) {
            continue;
        }
        let length = mount_path.components().count();
        if best
            .as_ref()
            .is_none_or(|(best_len, _, _)| length >= *best_len)
        {
            best = Some((length, fs_type.to_string(), mount_point.to_string()));
        }
    }
    let (_, fs_type, mount_point) = best?;
    let sensitivity = match fs_type.as_str() {
        "vfat" | "msdos" | "exfat" | "ntfs" | "ntfs3" | "cifs" | "smbfs" | "iso9660" | "udf" => {
            CaseSensitivity::Insensitive
        }
        "ext2" | "ext3" | "ext4" | "xfs" | "btrfs" | "tmpfs" | "ramfs" | "overlay" | "f2fs"
        | "reiserfs" | "jfs" | "zfs" | "ubifs" | "erofs" => CaseSensitivity::Sensitive,
        _ => return None,
    };
    Some(CaseProbe {
        sensitivity,
        method: format!("mount_table:{fs_type}"),
        evidence: Some(mount_point),
    })
}

fn flip_first_ascii_letter(name: &str) -> Option<String> {
    let mut output = String::with_capacity(name.len());
    let mut flipped = false;
    for ch in name.chars() {
        if !flipped {
            if ch.is_ascii_alphabetic() {
                output.push(if ch.is_ascii_lowercase() {
                    ch.to_ascii_uppercase()
                } else {
                    ch.to_ascii_lowercase()
                });
                flipped = true;
                continue;
            }
            if !ch.is_ascii() {
                return None;
            }
        }
        output.push(ch);
    }
    flipped.then_some(output)
}

fn unescape_mount_field(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}
