//! Linux backend: raw `getdents64` via `rustix::fs::RawDir` plus an fd-relative `statx` per
//! entry with a minimal mask and `AT_STATX_DONT_SYNC` (no path re-resolution, no remote sync).

use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;

use bclean_tree::{EntryKind, NodeFlags};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, RawDir, StatxFlags};

use crate::listing::{Listing, RawEntry};

/// `getdents64` buffer size per worker.
const BUF_BYTES: usize = 64 * 1024;

const MASK: StatxFlags = StatxFlags::TYPE
    .union(StatxFlags::NLINK)
    .union(StatxFlags::INO)
    .union(StatxFlags::SIZE)
    .union(StatxFlags::BLOCKS)
    .union(StatxFlags::MTIME);

/// Per-worker `getdents64` + `statx` reader.
pub struct LinuxReader {
    buf: Vec<MaybeUninit<u8>>,
}

const fn kind_of(ft: FileType) -> EntryKind {
    match ft {
        FileType::RegularFile => EntryKind::File,
        FileType::Directory => EntryKind::Dir,
        FileType::Symlink => EntryKind::Symlink,
        _ => EntryKind::Other,
    }
}

impl LinuxReader {
    pub fn new() -> Self {
        let mut buf = Vec::with_capacity(BUF_BYTES);
        buf.resize(BUF_BYTES, MaybeUninit::uninit());
        Self { buf }
    }

    /// Lists `dir` with `getdents64` + `statx` and returns the open directory descriptor.
    ///
    /// When `parent` is the open descriptor of `dir`'s parent, the directory is opened with
    /// `openat(parent, file_name)` instead of re-resolving the absolute path.
    ///
    /// # Errors
    ///
    /// * If the directory cannot be opened or `getdents64` fails
    pub fn list(
        &mut self,
        dir: &Path,
        parent: Option<&OwnedFd>,
        out: &mut Listing,
    ) -> io::Result<OwnedFd> {
        const FLAGS: OFlags = OFlags::RDONLY
            .union(OFlags::DIRECTORY)
            .union(OFlags::CLOEXEC);
        let fd = match (parent, dir.file_name()) {
            (Some(parent), Some(name)) => {
                rustix::fs::openat(parent, name, FLAGS | OFlags::NOFOLLOW, Mode::empty())?
            }
            _ => rustix::fs::open(dir, FLAGS, Mode::empty())?,
        };
        let mut raw = RawDir::new(fd.as_fd(), &mut self.buf);
        while let Some(item) = raw.next() {
            let item = item?;
            let name = item.file_name();
            let bytes = name.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            let stat = rustix::fs::statx(
                fd.as_fd(),
                name,
                AtFlags::SYMLINK_NOFOLLOW | AtFlags::STATX_DONT_SYNC,
                MASK,
            );
            let entry = stat.map_or_else(
                |_| {
                    let mut e = RawEntry::new(kind_of(item.file_type()));
                    e.flags = NodeFlags::ERROR;
                    e
                },
                |st| {
                    let mut e = RawEntry::new(kind_of(FileType::from_raw_mode(st.stx_mode.into())));
                    e.apparent = st.stx_size;
                    e.allocated = st.stx_blocks.saturating_mul(512);
                    e.mtime = st.stx_mtime.tv_sec;
                    e.ino = st.stx_ino;
                    e.dev = rustix::fs::makedev(st.stx_dev_major, st.stx_dev_minor);
                    e.nlink = st.stx_nlink;
                    e
                },
            );
            out.push(bytes, entry);
        }
        Ok(fd)
    }
}
