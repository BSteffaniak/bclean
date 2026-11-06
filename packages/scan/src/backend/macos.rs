//! macOS backend: `getattrlistbulk(2)` returns names and metadata for a whole batch of
//! directory entries per syscall, avoiding one `lstat` per entry.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use bclean_tree::{EntryKind, NodeFlags};

use crate::listing::{Listing, RawEntry};

/// `ATTR_CMN_ERROR` (not exported by `libc`): per-entry error code, packed right after the
/// returned attribute set.
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
/// Number of attribute groups in `attrlist`.
const ATTR_BIT_MAP_COUNT: u16 = 5;

const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;

const COMMON: u32 = libc::ATTR_CMN_RETURNED_ATTRS
    | ATTR_CMN_ERROR
    | libc::ATTR_CMN_NAME
    | libc::ATTR_CMN_DEVID
    | libc::ATTR_CMN_OBJTYPE
    | libc::ATTR_CMN_MODTIME
    | libc::ATTR_CMN_FILEID;
const DIR: u32 = libc::ATTR_DIR_MOUNTSTATUS | libc::ATTR_DIR_ALLOCSIZE | libc::ATTR_DIR_DATALENGTH;
const FILE: u32 =
    libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE | libc::ATTR_FILE_DATALENGTH;

/// Scratch buffer size per worker (in `u64` words, i.e. 256 KiB).
const BUF_WORDS: usize = 32 * 1024;

/// Per-worker `getattrlistbulk` reader.
pub struct MacReader {
    buf: Vec<u64>,
}

/// Bounds-checked little cursor over one packed attribute record.
struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let bytes = self.buf.get(self.pos..self.pos + N)?;
        self.pos += N;
        bytes.try_into().ok()
    }

    fn u32(&mut self) -> Option<u32> {
        self.take::<4>().map(u32::from_ne_bytes)
    }

    fn i32(&mut self) -> Option<i32> {
        self.take::<4>().map(i32::from_ne_bytes)
    }

    fn i64(&mut self) -> Option<i64> {
        self.take::<8>().map(i64::from_ne_bytes)
    }

    fn u64(&mut self) -> Option<u64> {
        self.take::<8>().map(u64::from_ne_bytes)
    }
}

fn non_negative(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

impl MacReader {
    pub fn new() -> Self {
        Self {
            buf: vec![0; BUF_WORDS],
        }
    }

    /// Lists `dir` with `getattrlistbulk` and returns the open directory descriptor.
    ///
    /// When `parent` is the open descriptor of `dir`'s parent, the directory is opened with
    /// `openat(parent, file_name)`, which avoids re-resolving (and contending on) every
    /// component of the absolute path.
    ///
    /// # Errors
    ///
    /// * If the directory cannot be opened or `getattrlistbulk` fails
    pub fn list(
        &mut self,
        dir: &Path,
        parent: Option<&OwnedFd>,
        out: &mut Listing,
    ) -> io::Result<OwnedFd> {
        let fd = open_dir(dir, parent)?;
        let mut attrs = libc::attrlist {
            bitmapcount: ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: COMMON,
            volattr: 0,
            dirattr: DIR,
            fileattr: FILE,
            forkattr: 0,
        };
        let byte_len = self.buf.len() * std::mem::size_of::<u64>();
        loop {
            // SAFETY: `attrs` is a valid `attrlist`, the buffer pointer is valid for
            // `byte_len` writable bytes (and 8-byte aligned), and `fd` is an open directory.
            let n = unsafe {
                libc::getattrlistbulk(
                    fd.as_raw_fd(),
                    (&raw mut attrs).cast(),
                    self.buf.as_mut_ptr().cast(),
                    byte_len,
                    0,
                )
            };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if n == 0 {
                return Ok(fd);
            }
            // SAFETY: the buffer holds `byte_len` initialized bytes (it is a `Vec<u64>` of
            // zero-initialized words) and `u8` has no alignment requirement.
            let bytes =
                unsafe { std::slice::from_raw_parts(self.buf.as_ptr().cast::<u8>(), byte_len) };
            parse_batch(bytes, n.unsigned_abs(), out)?;
        }
    }
}

fn open_dir(dir: &Path, parent: Option<&OwnedFd>) -> io::Result<OwnedFd> {
    const FLAGS: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
    let cstr = |bytes: &[u8]| {
        CString::new(bytes).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
    };
    let raw = if let (Some(parent), Some(name)) = (parent, dir.file_name()) {
        let name = cstr(name.as_bytes())?;
        // SAFETY: `parent` is an open directory descriptor and `name` a valid NUL-terminated
        // string; both outlive the call.
        unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), FLAGS | libc::O_NOFOLLOW) }
    } else {
        let path = cstr(dir.as_os_str().as_bytes())?;
        // SAFETY: `path` is a valid NUL-terminated string that outlives the call.
        unsafe { libc::open(path.as_ptr(), FLAGS) }
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a freshly opened descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn corrupt() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "malformed getattrlistbulk record",
    )
}

fn parse_batch(bytes: &[u8], count: u32, out: &mut Listing) -> io::Result<()> {
    let mut pos = 0usize;
    for _ in 0..count {
        let len_bytes: [u8; 4] = bytes
            .get(pos..pos + 4)
            .and_then(|b| b.try_into().ok())
            .ok_or_else(corrupt)?;
        let len = u32::from_ne_bytes(len_bytes) as usize;
        if len < 4 || pos + len > bytes.len() {
            return Err(corrupt());
        }
        parse_entry(&bytes[pos..pos + len], out).ok_or_else(corrupt)?;
        pos += len;
    }
    Ok(())
}

fn parse_entry(rec: &[u8], out: &mut Listing) -> Option<()> {
    let mut c = Cursor { buf: rec, pos: 4 };
    let common = c.u32()?;
    let _vol = c.u32()?;
    let dir = c.u32()?;
    let file = c.u32()?;
    let _fork = c.u32()?;

    let mut entry = RawEntry::new(EntryKind::Other);
    if common & ATTR_CMN_ERROR != 0 && c.u32()? != 0 {
        entry.flags = NodeFlags::ERROR;
    }
    let name: &[u8] = if common & libc::ATTR_CMN_NAME == 0 {
        &[]
    } else {
        let at = c.pos;
        let off = c.i32()?;
        let len = c.u32()? as usize;
        let start = at.checked_add_signed(isize::try_from(off).ok()?)?;
        let raw = rec.get(start..start + len)?;
        // The length includes the trailing NUL.
        raw.strip_suffix(&[0]).unwrap_or(raw)
    };
    if common & libc::ATTR_CMN_DEVID != 0 {
        entry.dev = u64::from_ne_bytes(i64::from(c.i32()?).to_ne_bytes());
    }
    let objtype = if common & libc::ATTR_CMN_OBJTYPE == 0 {
        0
    } else {
        c.u32()?
    };
    if common & libc::ATTR_CMN_MODTIME != 0 {
        entry.mtime = c.i64()?;
        let _nsec = c.i64()?;
    }
    if common & libc::ATTR_CMN_FILEID != 0 {
        entry.ino = c.u64()?;
    }
    entry.kind = match objtype {
        VREG => EntryKind::File,
        VDIR => EntryKind::Dir,
        VLNK => EntryKind::Symlink,
        _ => EntryKind::Other,
    };

    if dir & libc::ATTR_DIR_MOUNTSTATUS != 0 {
        entry.mount_point = c.u32()? & libc::DIR_MNTSTATUS_MNTPOINT != 0;
    }
    if dir & libc::ATTR_DIR_ALLOCSIZE != 0 {
        entry.allocated = non_negative(c.i64()?);
    }
    if dir & libc::ATTR_DIR_DATALENGTH != 0 {
        entry.apparent = non_negative(c.i64()?);
    }
    if file & libc::ATTR_FILE_LINKCOUNT != 0 {
        entry.nlink = c.u32()?;
    }
    if file & libc::ATTR_FILE_ALLOCSIZE != 0 {
        entry.allocated = non_negative(c.i64()?);
    }
    if file & libc::ATTR_FILE_DATALENGTH != 0 {
        entry.apparent = non_negative(c.i64()?);
    }
    if name.is_empty() || name == b"." || name == b".." {
        return Some(());
    }
    out.push(name, entry);
    Some(())
}
