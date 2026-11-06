//! Directory enumeration backends.
//!
//! Every backend lists one whole directory into a [`Listing`] including per-entry metadata,
//! without following symlinks. Native backends fetch metadata in bulk with the enumeration
//! (macOS `getattrlistbulk`, Windows `FileFullDirectoryInfo`) or with a cheap fd-relative
//! `statx` (Linux); the portable backend uses `std::fs`.

use std::io;
use std::path::Path;

use bclean_model::ScanBackend;
use bclean_tree::EntryKind;

use crate::listing::Listing;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod portable;
#[cfg(windows)]
mod windows;

/// `true` if this platform has a native bulk-metadata backend.
pub const HAS_NATIVE: bool = cfg!(any(target_os = "macos", target_os = "linux", windows));

/// Resolves a requested backend to the one that will actually run
/// ([`ScanBackend::Native`] or [`ScanBackend::Portable`]).
///
/// * `Auto` and `Native` resolve to `Native` where available, else `Portable`.
/// * `Mft` is not implemented yet and falls back like `Native`.
#[must_use]
pub const fn resolve(requested: ScanBackend) -> ScanBackend {
    match requested {
        ScanBackend::Portable => ScanBackend::Portable,
        ScanBackend::Auto | ScanBackend::Native | ScanBackend::Mft => {
            if HAS_NATIVE {
                ScanBackend::Native
            } else {
                ScanBackend::Portable
            }
        }
    }
}

/// Open directory handle that children can be opened relative to.
#[cfg(unix)]
pub type DirHandle = std::os::fd::OwnedFd;

/// Open directory handle (never produced on this platform).
#[cfg(not(unix))]
#[derive(Debug)]
pub enum DirHandle {}

/// Per-worker directory reader holding reusable scratch buffers.
pub enum Reader {
    /// `std::fs` based reader.
    Portable,
    /// `getattrlistbulk` reader.
    #[cfg(target_os = "macos")]
    Mac(macos::MacReader),
    /// `getdents64` + `statx` reader.
    #[cfg(target_os = "linux")]
    Linux(linux::LinuxReader),
    /// `GetFileInformationByHandleEx(FileFullDirectoryInfo)` reader.
    #[cfg(windows)]
    Windows(windows::WinReader),
}

impl Reader {
    /// Creates a reader for a resolved backend (see [`resolve`]).
    pub fn new(backend: ScanBackend) -> Self {
        match backend {
            #[cfg(target_os = "macos")]
            ScanBackend::Native => Self::Mac(macos::MacReader::new()),
            #[cfg(target_os = "linux")]
            ScanBackend::Native => Self::Linux(linux::LinuxReader::new()),
            #[cfg(windows)]
            ScanBackend::Native => Self::Windows(windows::WinReader::new()),
            _ => Self::Portable,
        }
    }

    /// Appends every entry of `dir` (except `.` and `..`) to `out`.
    ///
    /// `parent` is an open handle of `dir`'s parent directory if one is available; backends
    /// that support it open `dir` relative to it. Returns this directory's open handle when the
    /// backend has one, so the caller can pass it on to the children.
    ///
    /// On error, entries listed before the failure are kept in `out`.
    ///
    /// # Errors
    ///
    /// * If the directory cannot be opened or enumeration fails part-way
    #[cfg_attr(
        not(any(target_os = "macos", target_os = "linux")),
        allow(unused_variables, clippy::unnecessary_wraps)
    )] // `parent` is only used by the unix native readers.
    pub fn list(
        &mut self,
        dir: &Path,
        parent: Option<&DirHandle>,
        out: &mut Listing,
    ) -> io::Result<Option<DirHandle>> {
        match self {
            Self::Portable => portable::list(dir, out).map(|()| None),
            #[cfg(target_os = "macos")]
            Self::Mac(r) => r.list(dir, parent, out).map(Some),
            #[cfg(target_os = "linux")]
            Self::Linux(r) => r.list(dir, parent, out).map(Some),
            #[cfg(windows)]
            Self::Windows(r) => r.list(dir, out).map(|()| None),
        }
    }
}

/// Metadata of the scan root itself.
#[derive(Debug, Clone, Copy)]
pub struct RootStat {
    /// Kind of the root (symlinks given as root are followed).
    pub kind: EntryKind,
    /// Apparent size.
    pub apparent: u64,
    /// Allocated size.
    pub allocated: u64,
    /// Modification time (seconds since the Unix epoch).
    pub mtime: i64,
    /// Device id in the encoding the backends use for entries.
    pub dev: u64,
}

/// Stats the scan root, following symlinks.
///
/// # Errors
///
/// * If the root does not exist or cannot be stat'ed
pub fn stat_root(path: &Path) -> io::Result<RootStat> {
    let md = std::fs::metadata(path)?;
    Ok(portable::root_stat(&md))
}
