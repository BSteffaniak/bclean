//! Windows backend: `GetFileInformationByHandleEx(FileFullDirectoryInfo)` returns names, sizes
//! (`EndOfFile`, `AllocationSize`), attributes and timestamps for a batch of entries per call, so
//! no per-file open or stat is needed.
//!
//! Hard-link counts are not part of the enumeration data, so hard links are not deduplicated
//! by this backend. Reparse points that are symlinks or junctions/mount points are reported as
//! [`EntryKind::Symlink`] and never followed (which also keeps the walk on one volume).

use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

use bclean_tree::EntryKind;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_NO_MORE_FILES, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FULL_DIR_INFO, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FileFullDirectoryInfo, FileFullDirectoryRestartInfo,
    GetFileInformationByHandleEx, OPEN_EXISTING,
};

use crate::listing::{Listing, RawEntry};

/// `IO_REPARSE_TAG_MOUNT_POINT` (junctions and volume mount points).
const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
/// `IO_REPARSE_TAG_SYMLINK`.
const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;

/// Enumeration buffer size per worker (in `u64` words, i.e. 64 KiB).
const BUF_WORDS: usize = 8 * 1024;

/// Byte offset of `FileName` within `FILE_FULL_DIR_INFO`.
const NAME_OFFSET: usize = std::mem::offset_of!(FILE_FULL_DIR_INFO, FileName);

/// Per-worker directory enumeration reader.
pub struct WinReader {
    buf: Vec<u64>,
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a valid handle we opened and exclusively own.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// Converts a FILETIME (100ns intervals since 1601) into seconds since the Unix epoch.
const fn filetime_to_unix(ft: i64) -> i64 {
    ft.div_euclid(10_000_000) - 11_644_473_600
}

fn non_negative(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

impl WinReader {
    pub fn new() -> Self {
        Self {
            buf: vec![0; BUF_WORDS],
        }
    }

    /// Lists `dir` with `GetFileInformationByHandleEx`.
    ///
    /// # Errors
    ///
    /// * If the directory cannot be opened or enumeration fails
    pub fn list(&mut self, dir: &Path, out: &mut Listing) -> io::Result<()> {
        let wide = verbatim_wide(dir);
        // SAFETY: `wide` is a valid NUL-terminated UTF-16 string; all other arguments are
        // plain flags or null pointers accepted by `CreateFileW`.
        let raw = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let handle = Handle(raw);
        let byte_len = self.buf.len() * std::mem::size_of::<u64>();
        let mut class = FileFullDirectoryRestartInfo;
        loop {
            // SAFETY: `handle` is an open directory handle and the buffer is valid for
            // `byte_len` writable bytes with 8-byte alignment.
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    handle.0,
                    class,
                    self.buf.as_mut_ptr().cast(),
                    u32::try_from(byte_len).unwrap_or(u32::MAX),
                )
            };
            if ok == 0 {
                // SAFETY: trivially safe; reads the calling thread's last-error value.
                let code = unsafe { GetLastError() };
                if code == ERROR_NO_MORE_FILES {
                    return Ok(());
                }
                return Err(io::Error::from_raw_os_error(
                    i32::try_from(code).unwrap_or(i32::MAX),
                ));
            }
            class = FileFullDirectoryInfo;
            // SAFETY: the buffer holds `byte_len` initialized bytes (zero-initialized words).
            let bytes =
                unsafe { std::slice::from_raw_parts(self.buf.as_ptr().cast::<u8>(), byte_len) };
            parse_batch(bytes, out);
        }
    }
}

/// Encodes `dir` as a NUL-terminated wide string, adding the `\\?\` verbatim prefix to
/// absolute drive and UNC paths so paths longer than `MAX_PATH` can be opened. The scanner only
/// passes normalized absolute paths (no `.`/`..` components), which verbatim paths require.
fn verbatim_wide(dir: &Path) -> Vec<u16> {
    let raw: Vec<u16> = dir.as_os_str().encode_wide().collect();
    let w = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
    let is_sep = |c: u16| c == u16::from(b'\\') || c == u16::from(b'/');
    let mut out = if raw.starts_with(&w(r"\\?\")) || raw.starts_with(&w(r"\\.\")) {
        raw
    } else if raw.len() >= 3 && raw[1] == u16::from(b':') && is_sep(raw[2]) {
        [w(r"\\?\"), raw].concat()
    } else if raw.len() >= 2 && is_sep(raw[0]) && is_sep(raw[1]) {
        [w(r"\\?\UNC\"), raw[2..].to_vec()].concat()
    } else {
        raw
    };
    for c in &mut out {
        if *c == u16::from(b'/') {
            *c = u16::from(b'\\');
        }
    }
    out.push(0);
    out
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    bytes
        .get(at..at + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
}

fn read_i64(bytes: &[u8], at: usize) -> Option<i64> {
    bytes
        .get(at..at + 8)
        .and_then(|b| b.try_into().ok())
        .map(i64::from_le_bytes)
}

fn parse_batch(bytes: &[u8], out: &mut Listing) {
    let mut pos = 0usize;
    while let Some(next) = parse_entry(bytes, pos, out) {
        if next == 0 {
            break;
        }
        pos += next;
    }
}

/// Parses one record at `pos`; returns its `NextEntryOffset`.
fn parse_entry(bytes: &[u8], pos: usize, out: &mut Listing) -> Option<usize> {
    use std::mem::offset_of;
    let field = |off: usize| pos + off;
    let next = read_u32(
        bytes,
        field(offset_of!(FILE_FULL_DIR_INFO, NextEntryOffset)),
    )? as usize;
    let mtime = read_i64(bytes, field(offset_of!(FILE_FULL_DIR_INFO, LastWriteTime)))?;
    let eof = read_i64(bytes, field(offset_of!(FILE_FULL_DIR_INFO, EndOfFile)))?;
    let alloc = read_i64(bytes, field(offset_of!(FILE_FULL_DIR_INFO, AllocationSize)))?;
    let attrs = read_u32(bytes, field(offset_of!(FILE_FULL_DIR_INFO, FileAttributes)))?;
    let name_len = read_u32(bytes, field(offset_of!(FILE_FULL_DIR_INFO, FileNameLength)))? as usize;
    // For reparse points, `EaSize` holds the reparse tag.
    let tag = read_u32(bytes, field(offset_of!(FILE_FULL_DIR_INFO, EaSize)))?;
    let name_bytes = bytes.get(field(NAME_OFFSET)..field(NAME_OFFSET) + name_len)?;
    let wide: Vec<u16> = name_bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    let dot = u16::from(b'.');
    if wide == [dot] || wide == [dot, dot] {
        return Some(next);
    }

    let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
    let link = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0
        && (tag == IO_REPARSE_TAG_SYMLINK || tag == IO_REPARSE_TAG_MOUNT_POINT);
    let kind = if link {
        EntryKind::Symlink
    } else if is_dir {
        EntryKind::Dir
    } else {
        EntryKind::File
    };
    let mut entry = RawEntry::new(kind);
    if !is_dir {
        entry.apparent = non_negative(eof);
        entry.allocated = non_negative(alloc);
    }
    entry.mtime = filetime_to_unix(mtime);
    let name = std::ffi::OsString::from_wide(&wide);
    out.push(name.as_encoded_bytes(), entry);
    Some(next)
}
