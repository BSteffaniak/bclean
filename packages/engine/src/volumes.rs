//! Drives indexed as roots of their own (`monitor.index_volumes`).

use std::path::{Path, PathBuf};

use bclean_model::VolumeStats;
use globset::{Glob, GlobSetBuilder};

use crate::{EngineError, Result, Settings};

/// Volumes smaller than this (EFI and boot partitions, recovery images) are never
/// auto-indexed.
const MIN_VOLUME_BYTES: u64 = 1 << 30;

/// Mount points that hold system plumbing rather than user data, never auto-indexed.
const SKIPPED_MOUNTS: &[&str] = &[
    "/boot",
    "/boot/*",
    "/efi",
    "/.snapshots",
    "/snap/*",
    "/var/lib/docker",
    "/var/lib/docker/*",
    "/var/lib/containers",
    "/var/lib/containers/*",
    "/nix/store",
    "/System/Volumes/*",
];

/// Drives to index besides `monitor.index_roots`, by mount point. Empty when
/// `monitor.index_volumes` is off or `one_filesystem` is set.
///
/// # Errors
///
/// * [`EngineError::Volume`] if the mount table cannot be read
/// * [`EngineError::Invalid`] if a `monitor.exclude_volumes` pattern is not a valid glob
pub fn volume_roots(settings: &Settings, one_filesystem: bool) -> Result<Vec<PathBuf>> {
    let monitor = &settings.config().monitor;
    if one_filesystem || !monitor.index_volumes {
        return Ok(Vec::new());
    }
    let volumes = bclean_watch::list_volumes()?;
    let exclude = monitor
        .exclude_volumes
        .iter()
        .map(|p| settings.expander.expand(p).unwrap_or_else(|_| p.clone()))
        .collect::<Vec<_>>();
    indexable(&volumes, &settings.index_roots(), &exclude, same_device)
}

/// Mount points of `volumes` worth indexing as their own roots: not excluded, not system
/// plumbing, at least [`MIN_VOLUME_BYTES`], and not already holding one of `index_roots`
/// (an ancestor of a root, or the same device as one).
///
/// # Errors
///
/// * [`EngineError::Invalid`] if an `exclude` pattern is not a valid glob
pub fn indexable(
    volumes: &[VolumeStats],
    index_roots: &[PathBuf],
    exclude: &[String],
    same_device: impl Fn(&Path, &Path) -> bool,
) -> Result<Vec<PathBuf>> {
    let mut patterns = GlobSetBuilder::new();
    for pattern in SKIPPED_MOUNTS
        .iter()
        .copied()
        .map(str::to_owned)
        .chain(exclude.iter().cloned())
    {
        let glob = Glob::new(pattern.trim_end_matches(['/', '\\']))
            .map_err(|e| EngineError::Invalid(format!("monitor.exclude_volumes: {e}")))?;
        patterns.add(glob);
    }
    let patterns = patterns
        .build()
        .map_err(|e| EngineError::Invalid(format!("monitor.exclude_volumes: {e}")))?;
    let mut out: Vec<PathBuf> = volumes
        .iter()
        .filter(|v| v.total >= MIN_VOLUME_BYTES)
        .map(|v| PathBuf::from(&v.mount_point))
        .filter(|mount| !patterns.is_match(mount))
        .filter(|mount| {
            !index_roots
                .iter()
                .any(|root| root.starts_with(mount) || same_device(root, mount))
        })
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

#[cfg(unix)]
fn same_device(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev(),
        _ => false,
    }
}

/// Windows drives are told apart by path (`C:\` holds `C:\Users\me`).
#[cfg(not(unix))]
const fn same_device(_: &Path, _: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(mount: &str, total: u64) -> VolumeStats {
        VolumeStats {
            mount_point: mount.to_owned(),
            fs_type: "ext4".to_owned(),
            total,
            free: total / 2,
            available: total / 2,
            purgeable: None,
            at: 0,
        }
    }

    #[test]
    fn indexes_other_drives_but_not_system_or_excluded_ones() {
        const GIB: u64 = 1 << 30;
        let volumes = [
            volume("/", 500 * GIB),
            volume("/boot", 512 << 20),
            volume("/boot/efi", 2 * GIB),
            volume("/hdd", 4000 * GIB),
            volume("/home", 800 * GIB),
            volume("/mnt/backup", 2000 * GIB),
            volume("/run/media/me/USB", 64 * GIB),
            volume("/run/media/me/tiny", 100 << 20),
            volume("/srv", 100 * GIB),
        ];
        let roots = [PathBuf::from("/home/me")];
        let exclude = ["/mnt/backup/".to_owned()];
        // `/srv` lives on the same device as the home directory (say, a btrfs subvolume).
        let same =
            |root: &Path, mount: &Path| root.starts_with("/home") && mount == Path::new("/srv");
        let found = indexable(&volumes, &roots, &exclude, same).unwrap();
        assert_eq!(
            found,
            vec![PathBuf::from("/hdd"), PathBuf::from("/run/media/me/USB")]
        );
        // Globs work too.
        let found = indexable(&volumes, &roots, &["/run/media/**".to_owned()], same).unwrap();
        assert_eq!(
            found,
            vec![PathBuf::from("/hdd"), PathBuf::from("/mnt/backup")]
        );
        assert!(indexable(&volumes, &roots, &["[".to_owned()], same).is_err());
    }
}
