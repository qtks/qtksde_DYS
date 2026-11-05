use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use walker_core::EntryKind;

use crate::error::IndexError;

/// Compact JSONL record. Short field names keep the intermediate work file
/// around ~1.5x the raw path bytes instead of ~3x with pretty field names.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WorkEntry {
    #[serde(rename = "p")]
    pub path: String,
    #[serde(rename = "k")]
    pub kind: EntryKind,
    #[serde(rename = "s")]
    pub size: u64,
    #[serde(rename = "m")]
    pub mtime: i64,
    #[serde(rename = "i")]
    pub inode: u64,
}

impl WorkEntry {
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Directory
    }
}

/// Side-car files for resumable writes. They live next to the SQLite database
/// so a crash leaves both the checkpoint and the serialized entry stream.
pub(crate) struct WorkFiles {
    pub jsonl: PathBuf,
    pub dirs: PathBuf,
}

impl WorkFiles {
    pub fn new(db_path: &Path, session_id: &str) -> Result<Self, IndexError> {
        let directory = db_path.with_extension("work");
        fs::create_dir_all(&directory)?;
        Ok(Self {
            jsonl: directory.join(format!("{session_id}.jsonl")),
            dirs: directory.join(format!("{session_id}.dirs")),
        })
    }

    /// Serialize the scanned entries in ascending byte order so the DB writer
    /// can consume them batch by batch and always insert parents first.
    /// Returns the byte offset where each batch starts (plus the final length),
    /// which lets the in-process writer reuse the same checkpoint offsets it
    /// would use after a crash-resume that reads the file back.
    pub fn write(
        &self,
        entries: &[WorkEntry],
        listed_directories: &[PathBuf],
        batch_size: usize,
    ) -> Result<Vec<u64>, IndexError> {
        let batch_size = batch_size.max(1);
        let mut offsets = Vec::with_capacity(entries.len() / batch_size + 2);
        offsets.push(0u64);
        let mut offset = 0u64;

        let temporary = self.jsonl.with_extension("jsonl.tmp");
        {
            let file = File::create(&temporary)?;
            let mut writer = BufWriter::with_capacity(256 * 1024, file);
            for (index, entry) in entries.iter().enumerate() {
                let line = serde_json::to_string(entry)?;
                writer.write_all(line.as_bytes())?;
                writer.write_all(b"\n")?;
                offset += line.len() as u64 + 1;
                if (index + 1) % batch_size == 0 {
                    offsets.push(offset);
                }
            }
            writer.flush()?;
        }
        if offsets.last() != Some(&offset) {
            offsets.push(offset);
        }
        fs::rename(&temporary, &self.jsonl)?;

        let temporary = self.dirs.with_extension("dirs.tmp");
        {
            let file = File::create(&temporary)?;
            let mut writer = BufWriter::with_capacity(64 * 1024, file);
            for directory in listed_directories {
                writeln!(writer, "{}", directory.to_string_lossy())?;
            }
            writer.flush()?;
        }
        fs::rename(&temporary, &self.dirs)?;
        Ok(offsets)
    }
}

pub(crate) fn read_listed_directories(path: &Path) -> Result<HashSet<String>, IndexError> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut listed = HashSet::new();
    for line in reader.lines() {
        let line = line?;
        if !line.is_empty() {
            listed.insert(line);
        }
    }
    Ok(listed)
}

pub(crate) fn open_batch_reader(path: &Path, offset: u64) -> Result<BufReader<File>, IndexError> {
    use std::io::Seek;
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(256 * 1024, file);
    reader.seek(std::io::SeekFrom::Start(offset))?;
    Ok(reader)
}

pub(crate) fn read_batch(
    reader: &mut BufReader<File>,
    batch_size: usize,
) -> Result<Vec<WorkEntry>, IndexError> {
    let mut entries = Vec::with_capacity(batch_size);
    while entries.len() < batch_size {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            break;
        }
        if line.trim_end().is_empty() {
            continue;
        }
        entries.push(serde_json::from_str(line.trim_end())?);
    }
    Ok(entries)
}
