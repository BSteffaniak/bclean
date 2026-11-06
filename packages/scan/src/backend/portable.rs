//! Portable `std::fs` backend: `read_dir` plus one `symlink_metadata` per entry.

use std::fs::{self, FileType, Metadata};
use std::io;
use std::path::Path;

use bclean_tree::{EntryKind, NodeFlags};

use super::RootStat;
use crate::listing::{Listing, RawEntry};

fn kind_of(ft: FileType) -> EntryKind {
    if ft.is_symlink() {
        EntryKind::Symlink
    } else if ft.is_dir() {
        EntryKind::Dir
    } else if ft.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

#[cfg(unix)]
fn fill(md: &Metadata, e: &mut RawEntry) {
    use std::os::unix::fs::MetadataExt;
    e.apparent = md.size();
    e.allocated = md.blocks().saturating_mul(512);
    e.mtime = md.mtime();
    e.ino = md.ino();
    e.dev = md.dev();
    e.nlink = u32::try_from(md.nlink()).unwrap_or(u32::MAX);
}

#[cfg(not(unix))]
fn fill(md: &Metadata, e: &mut RawEntry) {
    e.apparent = md.len();
    // Allocation size is not exposed portably; approximate with the logical size.
    e.allocated = md.len();
    e.mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
}

/// Builds the root stat from (symlink-following) metadata.
pub fn root_stat(md: &Metadata) -> RootStat {
    let mut e = RawEntry::new(kind_of(md.file_type()));
    fill(md, &mut e);
    RootStat {
        kind: e.kind,
        apparent: e.apparent,
        allocated: e.allocated,
        mtime: e.mtime,
        dev: e.dev,
    }
}

/// Lists `dir` with `std::fs`.
///
/// # Errors
///
/// * If the directory cannot be opened or iteration fails
pub fn list(dir: &Path, out: &mut Listing) -> io::Result<()> {
    let mut first_err = None;
    for item in fs::read_dir(dir)? {
        let item = match item {
            Ok(item) => item,
            Err(e) => {
                first_err.get_or_insert(e);
                continue;
            }
        };
        let name = item.file_name();
        // `DirEntry::metadata` does not traverse symlinks.
        let entry = item.metadata().map_or_else(
            |_| {
                let kind = item.file_type().map_or(EntryKind::Other, kind_of);
                let mut e = RawEntry::new(kind);
                e.flags = NodeFlags::ERROR;
                e
            },
            |md| {
                let mut e = RawEntry::new(kind_of(md.file_type()));
                fill(&md, &mut e);
                e
            },
        );
        out.push(name.as_encoded_bytes(), entry);
    }
    first_err.map_or(Ok(()), Err)
}
