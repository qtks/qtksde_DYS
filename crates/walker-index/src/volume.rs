use std::fs;
use std::path::Path;

use walker_core::device_id;

use crate::db::IndexDb;
use crate::error::IndexError;
use crate::model::{Volume, VolumeStatus};
use crate::timeutil::now_iso8601;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountProbe {
    /// Mount point exists and, when known, reports the recorded device id.
    Mounted,
    /// Mount point path no longer resolves (classic unplug on Linux).
    Missing,
    /// Path exists but belongs to another device: the mount point is still
    /// there but the disk underneath it is gone (or a different disk appeared).
    DeviceMismatch,
}

pub fn probe_mount(path: &Path, expected_device: Option<u64>) -> MountProbe {
    match fs::metadata(path) {
        Err(_) => MountProbe::Missing,
        Ok(metadata) => match expected_device {
            Some(expected) if device_id(&metadata).is_some_and(|actual| actual != expected) => {
                MountProbe::DeviceMismatch
            }
            _ => MountProbe::Mounted,
        },
    }
}

#[derive(Debug, Clone)]
pub enum VolumeEvent {
    NotIndexed {
        mount_path: String,
    },
    Unchanged {
        volume: Volume,
    },
    WentOffline {
        volume_id: String,
        files_marked_offline: u64,
        reason: &'static str,
    },
    CameOnline {
        volume_id: String,
        files_restored: u64,
    },
}

/// One polling step of the volume state machine.
///
/// Choice: polling instead of inotify. Inotify can report events on a mount
/// point directory but cannot reliably report the unmount itself, and USB
/// removals often leave a stale handle. A stat of the mount path plus the
/// recorded `st_dev` catches both "path vanished" and "underlying filesystem
/// changed". The cost is up to one polling interval of latency.
pub fn poll_volume(db: &mut IndexDb, mount_path: &Path) -> Result<VolumeEvent, IndexError> {
    let mount = mount_path.to_string_lossy().into_owned();
    let Some(volume) = db.volume_by_mount_path(&mount)? else {
        return Ok(VolumeEvent::NotIndexed { mount_path: mount });
    };
    let probe = probe_mount(mount_path, volume.device);
    let now = now_iso8601();

    match (probe, volume.status) {
        (MountProbe::Mounted, VolumeStatus::Offline) => {
            let (_, restored) = db.restore_volume(&volume.id, &now)?;
            Ok(VolumeEvent::CameOnline {
                volume_id: volume.id,
                files_restored: restored,
            })
        }
        (MountProbe::Missing, VolumeStatus::Online) => {
            let affected = db.mark_volume_offline(&volume.id, &now)?;
            Ok(VolumeEvent::WentOffline {
                volume_id: volume.id,
                files_marked_offline: affected,
                reason: "mount point missing",
            })
        }
        (MountProbe::DeviceMismatch, VolumeStatus::Online) => {
            let affected = db.mark_volume_offline(&volume.id, &now)?;
            Ok(VolumeEvent::WentOffline {
                volume_id: volume.id,
                files_marked_offline: affected,
                reason: "device id mismatch",
            })
        }
        _ => Ok(VolumeEvent::Unchanged { volume }),
    }
}
