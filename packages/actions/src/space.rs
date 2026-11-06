//! Volume free-space measurement, used to report the space an action actually freed.

use std::io;
use std::path::Path;

/// Source of volume free-space readings.
pub trait SpaceProbe: Send + Sync {
    /// Bytes available to the current user on the volume containing `path`.
    ///
    /// # Errors
    ///
    /// * The volume cannot be queried.
    fn free_space(&self, path: &Path) -> io::Result<u64>;
}

/// [`SpaceProbe`] backed by the operating system.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemSpaceProbe;

impl SpaceProbe for SystemSpaceProbe {
    fn free_space(&self, path: &Path) -> io::Result<u64> {
        free_space(path)
    }
}

/// Bytes available to the current user on the volume containing `path`.
///
/// If `path` does not exist (for example because it was just deleted) the nearest existing
/// ancestor is queried instead.
///
/// # Errors
///
/// * No ancestor of `path` exists.
/// * The underlying `statvfs` / `GetDiskFreeSpaceExW` call fails.
pub fn free_space(path: &Path) -> io::Result<u64> {
    let existing = path
        .ancestors()
        .find(|p| !p.as_os_str().is_empty() && std::fs::symlink_metadata(p).is_ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no existing ancestor"))?;
    platform::free_space(existing)
}

#[cfg(unix)]
mod platform {
    use std::ffi::CString;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    pub fn free_space(path: &Path) -> io::Result<u64> {
        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        // SAFETY: `statvfs` is a plain C struct for which all-zero bytes is valid.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c_path` is a valid NUL-terminated string and `stat` is a valid out pointer.
        let rc = unsafe { libc::statvfs(c_path.as_ptr(), &raw mut stat) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // Field widths differ between platforms; widen everything to u64.
        #[allow(clippy::useless_conversion, clippy::unnecessary_cast)]
        let (avail, frsize) = (u64::from(stat.f_bavail), stat.f_frsize as u64);
        Ok(avail.saturating_mul(frsize))
    }
}

#[cfg(windows)]
mod platform {
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    pub fn free_space(path: &Path) -> io::Result<u64> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut available: u64 = 0;
        // SAFETY: `wide` is NUL-terminated; output pointers are valid or null as allowed.
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &raw mut available,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(available)
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use std::io;
    use std::path::Path;

    pub fn free_space(_path: &Path) -> io::Result<u64> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "free space unsupported on this platform",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_free_space() {
        let dir = tempfile::tempdir().unwrap();
        assert!(free_space(dir.path()).unwrap() > 0);
        assert!(free_space(&dir.path().join("missing/child")).unwrap() > 0);
    }
}
