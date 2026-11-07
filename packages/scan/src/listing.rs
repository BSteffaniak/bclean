//! Backend-neutral representation of one directory listing.

use std::ffi::OsStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bclean_tree::{EntryKind, NodeFlags};

/// Sentinel for "no child token".
pub const NO_TOKEN: u32 = u32::MAX;

/// One entry produced by a backend for a single directory.
#[derive(Debug, Clone, Copy)]
pub struct RawEntry {
    /// Offset of the name in [`Listing::names`].
    pub name_off: u32,
    /// Length of the name in bytes (OS-encoded, see [`os_str`]).
    pub name_len: u32,
    /// Entry kind (symlinks are never followed).
    pub kind: EntryKind,
    /// Apparent (logical) size.
    pub apparent: u64,
    /// Allocated (on-disk) size.
    pub allocated: u64,
    /// Modification time in seconds since the Unix epoch.
    pub mtime: i64,
    /// Inode / file id (0 if unknown).
    pub ino: u64,
    /// Device id (0 if unknown; only compared within one backend).
    pub dev: u64,
    /// Hard link count (1 if unknown).
    pub nlink: u32,
    /// Backend positively knows this directory is a mount point.
    pub mount_point: bool,
    /// Flags decided by the backend and the walker.
    pub flags: NodeFlags,
    /// Walker-assigned token of a materialized subdirectory ([`NO_TOKEN`] otherwise).
    pub child: u32,
}

impl RawEntry {
    /// Entry with the given kind and everything else zeroed.
    pub const fn new(kind: EntryKind) -> Self {
        Self {
            name_off: 0,
            name_len: 0,
            kind,
            apparent: 0,
            allocated: 0,
            mtime: 0,
            ino: 0,
            dev: 0,
            nlink: 1,
            mount_point: false,
            flags: NodeFlags::EMPTY,
            child: NO_TOKEN,
        }
    }
}

/// Entries between two live progress reports of one listing.
const LIVE_CHUNK: u64 = 4096;

/// All entries of one directory with their names interned in a single buffer.
#[derive(Debug, Default)]
pub struct Listing {
    /// Interned OS-encoded name bytes.
    pub names: Vec<u8>,
    /// Entries in enumeration order.
    pub entries: Vec<RawEntry>,
    /// Counter that [`Listing::push`] adds to every [`LIVE_CHUNK`] entries while a large
    /// directory is being read, until [`Listing::settle`] takes the additions back.
    live: Option<Arc<AtomicU64>>,
    /// Entries added to `live` and not yet taken back.
    reported: u64,
}

impl Listing {
    /// Clears the listing, keeping capacity.
    pub fn clear(&mut self) {
        self.settle();
        self.names.clear();
        self.entries.clear();
    }

    /// Reports entries to `counter` while they are pushed (see [`Listing::settle`]).
    pub fn report_to(&mut self, counter: &Arc<AtomicU64>) {
        if self
            .live
            .as_ref()
            .is_none_or(|live| !Arc::ptr_eq(live, counter))
        {
            self.settle();
            self.live = Some(Arc::clone(counter));
        }
    }

    /// Takes back the entries reported while pushing (call once the listing is counted).
    pub fn settle(&mut self) {
        if self.reported > 0
            && let Some(live) = &self.live
        {
            live.fetch_sub(self.reported, Ordering::Relaxed);
        }
        self.reported = 0;
    }

    /// Appends `entry` with the given OS-encoded name bytes.
    pub fn push(&mut self, name: &[u8], mut entry: RawEntry) {
        entry.name_off = u32::try_from(self.names.len()).unwrap_or(u32::MAX);
        entry.name_len = u32::try_from(name.len()).unwrap_or(u32::MAX);
        self.names.extend_from_slice(name);
        self.entries.push(entry);
        if (self.entries.len() as u64).is_multiple_of(LIVE_CHUNK)
            && let Some(live) = &self.live
        {
            live.fetch_add(LIVE_CHUNK, Ordering::Relaxed);
            self.reported += LIVE_CHUNK;
        }
    }

    /// Name bytes of `entry`.
    pub fn name_bytes(&self, entry: &RawEntry) -> &[u8] {
        let start = entry.name_off as usize;
        &self.names[start..start + entry.name_len as usize]
    }

    /// Name of `entry` as an [`OsStr`].
    pub fn name(&self, entry: &RawEntry) -> &OsStr {
        os_str(self.name_bytes(entry))
    }
}

/// Reinterprets OS-encoded bytes (as produced by [`OsStr::as_encoded_bytes`]) as an [`OsStr`].
#[cfg(unix)]
pub fn os_str(bytes: &[u8]) -> &OsStr {
    std::os::unix::ffi::OsStrExt::from_bytes(bytes)
}

/// Reinterprets OS-encoded bytes (as produced by [`OsStr::as_encoded_bytes`]) as an [`OsStr`].
#[cfg(not(unix))]
pub fn os_str(bytes: &[u8]) -> &OsStr {
    // SAFETY: every name stored in a `Listing` comes from `OsStr::as_encoded_bytes` of a
    // complete `OsStr` produced on this platform in this process, so it is valid encoded data.
    unsafe { OsStr::from_encoded_bytes_unchecked(bytes) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_listings_report_live_progress_until_settled() {
        let live = Arc::new(AtomicU64::new(0));
        let mut listing = Listing::default();
        listing.report_to(&live);
        for _ in 0..(LIVE_CHUNK * 2 + 10) {
            listing.push(b"f", RawEntry::new(EntryKind::File));
        }
        assert_eq!(live.load(Ordering::Relaxed), LIVE_CHUNK * 2);
        listing.settle();
        assert_eq!(live.load(Ordering::Relaxed), 0);
        listing.clear();
        assert_eq!(live.load(Ordering::Relaxed), 0);
    }
}
