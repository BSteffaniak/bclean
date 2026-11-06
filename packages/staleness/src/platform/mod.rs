//! Platform-specific helpers with portable fallbacks.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

use std::path::Path;

/// Whether the volume containing `path` maintains access times.
#[cfg(target_os = "linux")]
pub fn atime_trusted(path: &Path) -> bool {
    linux::atime_trusted(path)
}

/// Whether the volume containing `path` maintains access times.
#[cfg(target_os = "macos")]
pub fn atime_trusted(path: &Path) -> bool {
    macos::atime_trusted(path)
}

/// Whether the volume containing `path` maintains access times.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[allow(clippy::missing_const_for_fn)] // must match the non-const platform implementations
pub fn atime_trusted(_path: &Path) -> bool {
    false
}

/// OS "last used" metadata for `path`: Unix seconds plus a description of the source.
#[cfg(target_os = "macos")]
pub fn os_last_used(path: &Path) -> Option<(i64, &'static str)> {
    macos::spotlight_last_used(path).map(|t| (t, "Spotlight kMDItemLastUsedDate"))
}

/// OS "last used" metadata for `path`: Unix seconds plus a description of the source.
#[cfg(target_os = "windows")]
pub fn os_last_used(path: &Path) -> Option<(i64, &'static str)> {
    windows::last_access(path).map(|t| (t, "NTFS last access time"))
}

/// OS "last used" metadata for `path`: none on this platform.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
#[allow(clippy::missing_const_for_fn)] // must match the non-const platform implementations
pub fn os_last_used(_path: &Path) -> Option<(i64, &'static str)> {
    None
}
