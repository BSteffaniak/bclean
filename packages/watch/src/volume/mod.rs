//! Mounted volume capacity.

use std::io;
use std::path::Path;

use bclean_model::VolumeStats;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

/// Errors from volume queries.
#[derive(Debug, thiserror::Error)]
pub enum VolumeError {
    /// An OS call failed.
    #[error("volume query failed: {0}")]
    Io(#[from] io::Error),
    /// Volume queries are not implemented on this platform.
    #[error("volume queries are not supported on this platform")]
    Unsupported,
}

/// Which volumes [`list_volumes_with`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(clippy::struct_excessive_bools)] // independent opt-in switches
pub struct VolumeFilter {
    /// Include RAM-backed filesystems (`tmpfs`, `ramfs`) on Linux.
    pub include_tmpfs: bool,
    /// Include network / remote filesystems.
    pub include_remote: bool,
    /// Include read-only volumes (system snapshots, images, optical media).
    pub include_read_only: bool,
    /// Include volumes reporting zero capacity.
    pub include_empty: bool,
}

/// Lists local, writable volumes with non-zero capacity, sorted by mount point.
///
/// # Errors
///
/// * [`VolumeError::Io`] if the mount table cannot be read
/// * [`VolumeError::Unsupported`] on platforms without an implementation
pub fn list_volumes() -> Result<Vec<VolumeStats>, VolumeError> {
    list_volumes_with(VolumeFilter::default())
}

/// Lists volumes matching `filter`, sorted by mount point.
///
/// # Errors
///
/// * [`VolumeError::Io`] if the mount table cannot be read
/// * [`VolumeError::Unsupported`] on platforms without an implementation
pub fn list_volumes_with(filter: VolumeFilter) -> Result<Vec<VolumeStats>, VolumeError> {
    #[cfg(target_os = "macos")]
    let mut volumes = macos::list(filter)?;
    #[cfg(target_os = "linux")]
    let mut volumes = linux::list(filter)?;
    #[cfg(windows)]
    let mut volumes = windows::list(filter)?;
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    let mut volumes: Vec<VolumeStats> = {
        let _ = filter;
        return Err(VolumeError::Unsupported);
    };
    volumes.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    Ok(volumes)
}

/// Returns the stats of the volume containing `path`, regardless of any [`VolumeFilter`].
///
/// # Errors
///
/// * [`VolumeError::Io`] if `path` does not exist or the OS query fails
/// * [`VolumeError::Unsupported`] on platforms without an implementation
pub fn volume_for(path: &Path) -> Result<VolumeStats, VolumeError> {
    #[cfg(target_os = "macos")]
    return macos::volume_for(path);
    #[cfg(target_os = "linux")]
    return linux::volume_for(path);
    #[cfg(windows)]
    return windows::volume_for(path);
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = path;
        Err(VolumeError::Unsupported)
    }
}

/// Bytes used by the volume mounted at `mount_point` alone, when it shares its capacity.
///
/// The volumes of an APFS container all report the container's usage through `statfs`, so
/// [`VolumeStats::used`] cannot tell the user data volume from the Nix store or the system
/// volume next to it. `None` where volumes don't share space, or if unknown.
#[cfg(target_os = "macos")]
#[must_use]
pub fn volume_own_used(mount_point: &Path) -> Option<u64> {
    macos::own_used(mount_point)
}

/// Bytes used by the volume mounted at `mount_point` alone, when it shares its capacity.
///
/// Only APFS volumes share capacity; elsewhere this is always `None`.
#[cfg(not(target_os = "macos"))]
#[must_use]
pub const fn volume_own_used(_mount_point: &Path) -> Option<u64> {
    None
}

/// Current Unix time in seconds.
#[allow(dead_code)] // unused on platforms without a volume backend
pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn temp_dir_volume_can_be_queried() {
        let dir = tempfile::tempdir().unwrap();
        let vol = volume_for(dir.path()).unwrap();
        assert!(vol.total > 0, "{vol:?}");
        assert!(vol.available <= vol.total);
        assert!(vol.free <= vol.total);
        assert_ne!(vol.fs_type, "");
        // macOS firmlinks (`/private/var` lives on `/System/Volumes/Data`) and Windows path
        // prefixes make a textual prefix check meaningless there.
        #[cfg(target_os = "linux")]
        assert!(
            dir.path()
                .canonicalize()
                .unwrap()
                .starts_with(&vol.mount_point)
        );
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn listed_volumes_are_sorted_and_queryable() {
        // A temp directory can live on an intentionally excluded mount (for
        // example macOS MNT_DONTBROWSE /nix, Linux tmpfs, or a remote mount).
        // Listing policy must not be inferred from the location of TMPDIR.
        let listed = list_volumes().unwrap();
        assert!(
            listed
                .windows(2)
                .all(|w| w[0].mount_point < w[1].mount_point)
        );
        for volume in listed {
            let queried = volume_for(Path::new(&volume.mount_point)).unwrap();
            assert_eq!(queried.mount_point, volume.mount_point);
            assert_eq!(queried.fs_type, volume.fs_type);
            assert!(volume.total > 0);
            assert!(volume.available <= volume.total);
            assert!(volume.free <= volume.total);
        }
    }
}
