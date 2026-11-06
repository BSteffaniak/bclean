//! Windows volumes via `GetLogicalDriveStringsW`, `GetDiskFreeSpaceExW`, and
//! `GetVolumeInformationW`.
//!
//! Only drive-letter volumes are listed (folder mount points are not enumerated). Optical
//! drives and read-only volumes require [`VolumeFilter::include_read_only`]; network drives
//! require [`VolumeFilter::include_remote`]; RAM disks require [`VolumeFilter::include_tmpfs`].

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::Path;

use bclean_model::VolumeStats;
use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDriveStringsW, GetVolumeInformationW,
    GetVolumePathNameW,
};

use super::{VolumeError, VolumeFilter, now};

// `GetDriveTypeW` results (WinBase.h).
const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_REMOTE: u32 = 4;
const DRIVE_CDROM: u32 = 5;
const DRIVE_RAMDISK: u32 = 6;
// File system flag (WinNT.h).
const FILE_READ_ONLY_VOLUME: u32 = 0x0008_0000;

pub fn list(filter: VolumeFilter) -> Result<Vec<VolumeStats>, VolumeError> {
    let mut buf = vec![0_u16; 512];
    // SAFETY: `buf` is writable for `buf.len()` UTF-16 units.
    let n =
        unsafe { GetLogicalDriveStringsW(u32::try_from(buf.len()).unwrap_or(0), buf.as_mut_ptr()) };
    if n == 0 {
        return Err(io::Error::last_os_error().into());
    }
    buf.truncate((n as usize).min(buf.len()));
    let at = now();
    let mut out = Vec::new();
    for root in buf.split(|c| *c == 0).filter(|s| !s.is_empty()) {
        let mut wide = root.to_vec();
        wide.push(0);
        // SAFETY: `wide` is a NUL-terminated UTF-16 string.
        let drive_type = unsafe { GetDriveTypeW(wide.as_ptr()) };
        let allowed = match drive_type {
            DRIVE_FIXED | DRIVE_REMOVABLE => true,
            DRIVE_REMOTE => filter.include_remote,
            DRIVE_RAMDISK => filter.include_tmpfs,
            DRIVE_CDROM => filter.include_read_only,
            _ => false,
        };
        if !allowed {
            continue;
        }
        // Removable drives without media fail here; skip them.
        let Ok((stats, flags)) = stats_for_root(&wide, at) else {
            continue;
        };
        if flags & FILE_READ_ONLY_VOLUME != 0 && !filter.include_read_only {
            continue;
        }
        if stats.total > 0 || filter.include_empty {
            out.push(stats);
        }
    }
    Ok(out)
}

pub fn volume_for(path: &Path) -> Result<VolumeStats, VolumeError> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut root = vec![0_u16; wide.len().max(261) + 1];
    // SAFETY: `wide` is NUL-terminated and `root` is writable for `root.len()` units.
    let ok = unsafe {
        GetVolumePathNameW(
            wide.as_ptr(),
            root.as_mut_ptr(),
            u32::try_from(root.len()).unwrap_or(0),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error().into());
    }
    let len = root.iter().position(|c| *c == 0).unwrap_or(root.len());
    root.truncate(len);
    root.push(0);
    Ok(stats_for_root(&root, now())?.0)
}

/// Queries capacity and file system name for a NUL-terminated volume root like `C:\`.
fn stats_for_root(root: &[u16], at: i64) -> io::Result<(VolumeStats, u32)> {
    let (mut avail, mut total, mut free) = (0_u64, 0_u64, 0_u64);
    // SAFETY: `root` is NUL-terminated and the out pointers reference live u64s.
    if unsafe { GetDiskFreeSpaceExW(root.as_ptr(), &raw mut avail, &raw mut total, &raw mut free) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut fs_name = [0_u16; 64];
    let mut flags = 0_u32;
    // SAFETY: `root` is NUL-terminated; unused out-parameters are null; `fs_name` is writable for
    // its full length.
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut flags,
            fs_name.as_mut_ptr(),
            u32::try_from(fs_name.len()).unwrap_or(0),
        )
    };
    let fs_type = if ok == 0 {
        String::new()
    } else {
        let len = fs_name
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(fs_name.len());
        String::from_utf16_lossy(&fs_name[..len]).to_ascii_lowercase()
    };
    let mount = &root[..root.len().saturating_sub(1)];
    let mount_point = OsString::from_wide(mount).to_string_lossy().into_owned();
    Ok((
        VolumeStats {
            mount_point,
            fs_type,
            total,
            free,
            available: avail,
            purgeable: None,
            at,
        },
        flags,
    ))
}
