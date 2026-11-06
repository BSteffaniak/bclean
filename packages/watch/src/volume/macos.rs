//! macOS volumes via `getfsstat(2)` / `statfs(2)`.
//!
//! Filtering rules:
//!
//! * Non-local (`!MNT_LOCAL`) volumes are remote unless [`VolumeFilter::include_remote`].
//! * `devfs`, `autofs`, and other pseudo filesystems are always skipped.
//! * `MNT_DONTBROWSE` volumes (the hidden APFS `VM`, `Preboot`, `Update`, `xarts`, `Hardware`
//!   ... volumes) are skipped, except the user data volume `/System/Volumes/Data`.
//! * Read-only volumes are skipped by default. This drops `/`, which on macOS 11+ is a sealed,
//!   read-only snapshot of the system volume sharing its APFS container (and therefore its
//!   free space) with `/System/Volumes/Data`, where all user data actually lives.
//!
//! Purgeable space is only exposed through Foundation (`NSURLVolumeAvailableCapacityFor...`),
//! so [`VolumeStats::purgeable`] is currently always `None`.

use std::ffi::{CStr, CString, c_char};
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use bclean_model::VolumeStats;

use super::{VolumeError, VolumeFilter, now};

const DATA_VOLUME: &str = "/System/Volumes/Data";
const PSEUDO_FS: &[&str] = &["devfs", "autofs", "nullfs", "fdesc", "lifs"];

pub fn list(filter: VolumeFilter) -> Result<Vec<VolumeStats>, VolumeError> {
    // SAFETY: a null buffer with size 0 only asks for the number of mounted filesystems.
    let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    let count = usize::try_from(count).map_err(|_| io::Error::last_os_error())?;
    // Leave headroom for mounts appearing between the two calls.
    let capacity = count + 8;
    let mut buf: Vec<libc::statfs> = Vec::with_capacity(capacity);
    let bytes = i32::try_from(capacity * std::mem::size_of::<libc::statfs>())
        .map_err(|_| io::Error::other("too many mounts"))?;
    // SAFETY: `buf` has room for `capacity` statfs records and `bytes` is exactly that size; the
    // kernel writes at most that many and returns how many it wrote.
    let got = unsafe { libc::getfsstat(buf.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
    let got = usize::try_from(got).map_err(|_| io::Error::last_os_error())?;
    // SAFETY: the kernel initialized the first `got <= capacity` records.
    unsafe { buf.set_len(got.min(capacity)) };

    let at = now();
    Ok(buf
        .iter()
        .filter(|s| keep(s, filter))
        .map(|s| convert(s, at))
        .collect())
}

pub fn volume_for(path: &Path) -> Result<VolumeStats, VolumeError> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    // SAFETY: an all-zero statfs is a valid value (plain integers and byte arrays).
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is NUL-terminated and `st` is a valid, writable statfs.
    if unsafe { libc::statfs(c_path.as_ptr(), &raw mut st) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(convert(&st, now()))
}

fn keep(s: &libc::statfs, filter: VolumeFilter) -> bool {
    let flags = s.f_flags;
    let fs_type = c_str(&s.f_fstypename);
    let mount = c_str(&s.f_mntonname);
    let has = |flag: i32| flags & flag.cast_unsigned() != 0;
    if PSEUDO_FS.contains(&fs_type.as_str()) {
        return false;
    }
    if !has(libc::MNT_LOCAL) && !filter.include_remote {
        return false;
    }
    if has(libc::MNT_DONTBROWSE) && mount != DATA_VOLUME {
        return false;
    }
    if has(libc::MNT_RDONLY) && !filter.include_read_only {
        return false;
    }
    s.f_blocks > 0 || filter.include_empty
}

fn convert(s: &libc::statfs, at: i64) -> VolumeStats {
    let bsize = u64::from(s.f_bsize);
    VolumeStats {
        mount_point: c_str(&s.f_mntonname),
        fs_type: c_str(&s.f_fstypename),
        total: s.f_blocks.saturating_mul(bsize),
        free: s.f_bfree.saturating_mul(bsize),
        available: s.f_bavail.saturating_mul(bsize),
        purgeable: None,
        at,
    }
}

fn c_str(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf.iter().map(|c| c.cast_unsigned()).collect();
    CStr::from_bytes_until_nul(&bytes).map_or_else(
        |_| String::from_utf8_lossy(&bytes).into_owned(),
        |s| s.to_string_lossy().into_owned(),
    )
}
