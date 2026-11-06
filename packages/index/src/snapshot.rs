//! Compact, versioned, checksummed binary snapshots of a [`Tree`].
//!
//! # Format (version 1)
//!
//! All integers are little-endian.
//!
//! ```text
//! magic          8 bytes   "BCLNSNAP"
//! version        u32       FORMAT_VERSION
//! header_len     u32       length of the header block that follows
//! header         header_len bytes:
//!     scanned_at   i64      Unix seconds of the scan
//!     backend      u32 len + UTF-8 bytes (scanner backend name)
//!     root_path    u32 len + path bytes (raw on Unix, UTF-8 elsewhere)
//!     has_cursor   u8 (0/1), then u32 len + opaque watch cursor bytes if 1
//!     node_count   u64
//! sections       one per array, in a fixed order (see `Section`):
//!     width        u8       element width in bytes (1, 4, or 8)
//!     count        u64      element count
//!     data         count * width bytes
//! checksum       u64       xxh3-64 of every preceding byte
//! ```
//!
//! `u64` arrays whose values all fit in 32 bits are stored with width 4, which roughly halves
//! the size of the offset/size/item columns for typical trees. Readers verify the checksum of
//! the whole file before decoding anything, so a torn or corrupted file is always rejected.

use std::fs::{self, File};
use std::io::{self, BufWriter, Read as _, Write};
use std::path::{Path, PathBuf};

use bclean_tree::{PartsError, Tree, TreeParts};
use xxhash_rust::xxh3::{Xxh3, xxh3_64};

use crate::pathbytes::{bytes_to_os, os_to_bytes};

/// File magic.
pub const MAGIC: [u8; 8] = *b"BCLNSNAP";
/// Current on-disk format version.
pub const FORMAT_VERSION: u32 = 1;

/// Elements converted per write chunk.
const CHUNK: usize = 64 * 1024;
/// Fixed prefix: magic + version + header length.
const PREFIX_LEN: usize = 16;
/// Trailing checksum length.
const CHECKSUM_LEN: usize = 8;
/// Upper bound on the header block (guards against absurd allocations on corrupt input).
const MAX_HEADER_LEN: u32 = 16 * 1024 * 1024;

/// Errors from reading or writing snapshots.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// Underlying I/O failure.
    #[error("snapshot I/O error: {0}")]
    Io(#[from] io::Error),
    /// The file does not start with the snapshot magic.
    #[error("not a bclean snapshot file")]
    NotASnapshot,
    /// The file was written by an unsupported format version.
    #[error("unsupported snapshot format version {0}")]
    UnsupportedVersion(u32),
    /// The checksum does not match the contents.
    #[error("snapshot checksum mismatch")]
    ChecksumMismatch,
    /// The file is structurally invalid.
    #[error("corrupt snapshot: {0}")]
    Corrupt(&'static str),
    /// The decoded arrays do not form a valid tree.
    #[error("invalid snapshot tree: {0}")]
    Parts(#[from] PartsError),
    /// The snapshot belongs to a different root than requested.
    #[error("snapshot root mismatch: expected {expected:?}, found {found:?}")]
    RootMismatch {
        /// Requested root.
        expected: PathBuf,
        /// Root recorded in the file.
        found: PathBuf,
    },
    /// A header field is too large to encode.
    #[error("snapshot field too large: {0}")]
    TooLarge(&'static str),
}

impl SnapshotError {
    /// Returns `true` if the file exists but is unusable (corrupt, foreign, or from another
    /// version) and should be discarded and rebuilt by rescanning. I/O errors return `false`.
    #[must_use]
    pub const fn is_invalid(&self) -> bool {
        !matches!(self, Self::Io(_) | Self::TooLarge(_))
    }
}

/// Metadata stored alongside a tree.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SnapshotMeta {
    /// Unix seconds at which the scan completed.
    pub scanned_at: i64,
    /// Name of the scanner backend that produced the tree.
    pub backend: String,
    /// Opaque, serialized watch cursor valid as of the snapshot (e.g. an `FSEvents` event id).
    pub cursor: Option<Vec<u8>>,
}

/// Snapshot header, readable without decoding the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotHeader {
    /// Format version of the file.
    pub version: u32,
    /// Root path of the stored tree.
    pub root_path: PathBuf,
    /// Number of node slots stored.
    pub node_count: u64,
    /// Stored metadata.
    pub meta: SnapshotMeta,
}

/// A loaded snapshot.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Stored metadata.
    pub meta: SnapshotMeta,
    /// The reconstructed tree (aggregates recomputed).
    pub tree: Tree,
}

/// Writes `parts` and `meta` to `path` atomically (temporary file, fsync, rename).
///
/// Returns the number of bytes written.
///
/// # Errors
///
/// * [`SnapshotError::Io`] if the temporary file cannot be written, synced, or renamed
/// * [`SnapshotError::TooLarge`] if a header string exceeds `u32::MAX` bytes
pub fn write_parts(
    path: &Path,
    parts: &TreeParts,
    meta: &SnapshotMeta,
) -> Result<u64, SnapshotError> {
    let header = encode_header(parts, meta)?;
    let tmp = temp_path(path);
    let result = (|| {
        let file = File::create(&tmp)?;
        let mut w = HashWriter::new(BufWriter::with_capacity(1 << 20, file));
        w.write_all(&MAGIC)?;
        w.write_all(&FORMAT_VERSION.to_le_bytes())?;
        let header_len =
            u32::try_from(header.len()).map_err(|_| SnapshotError::TooLarge("header"))?;
        w.write_all(&header_len.to_le_bytes())?;
        w.write_all(&header)?;
        write_sections(&mut w, parts)?;
        let digest = w.hasher.digest();
        w.write_all(&digest.to_le_bytes())?;
        let written = w.written;
        let file = w
            .inner
            .into_inner()
            .map_err(io::IntoInnerError::into_error)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        sync_parent(path);
        Ok(written)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Clones `tree` into parts and writes it with [`write_parts`].
///
/// # Errors
///
/// * Same as [`write_parts`]
pub fn write_tree(path: &Path, tree: &Tree, meta: &SnapshotMeta) -> Result<u64, SnapshotError> {
    write_parts(path, &tree.clone().into_parts(), meta)
}

/// Reads only the header of a snapshot file. The checksum is **not** verified.
///
/// # Errors
///
/// * [`SnapshotError::Io`] if the file cannot be read
/// * [`SnapshotError::NotASnapshot`] / [`SnapshotError::UnsupportedVersion`] for foreign files
/// * [`SnapshotError::Corrupt`] if the header is truncated or malformed
pub fn read_header(path: &Path) -> Result<SnapshotHeader, SnapshotError> {
    let mut file = File::open(path)?;
    let mut prefix = [0_u8; PREFIX_LEN];
    file.read_exact(&mut prefix).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            SnapshotError::NotASnapshot
        } else {
            e.into()
        }
    })?;
    let (version, header_len) = check_prefix(&prefix)?;
    let mut header = vec![0_u8; header_len as usize];
    file.read_exact(&mut header).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            SnapshotError::Corrupt("truncated header")
        } else {
            e.into()
        }
    })?;
    let mut cur = Cursor::new(&header);
    let (root_path, node_count, meta) = decode_header(&mut cur)?;
    Ok(SnapshotHeader {
        version,
        root_path,
        node_count,
        meta,
    })
}

/// Reads and verifies a snapshot, returning its header and raw parts.
///
/// # Errors
///
/// * [`SnapshotError::Io`] if the file cannot be read
/// * [`SnapshotError::NotASnapshot`] / [`SnapshotError::UnsupportedVersion`] for foreign files
/// * [`SnapshotError::ChecksumMismatch`] if the contents were corrupted
/// * [`SnapshotError::Corrupt`] if the structure is malformed
pub fn read_parts(path: &Path) -> Result<(SnapshotHeader, TreeParts), SnapshotError> {
    let data = fs::read(path)?;
    decode(&data)
}

/// Reads, verifies, and rebuilds a snapshot.
///
/// If `expected_root` is given, the stored root must match it exactly.
///
/// # Errors
///
/// * Everything [`read_parts`] returns
/// * [`SnapshotError::RootMismatch`] if the stored root differs from `expected_root`
/// * [`SnapshotError::Parts`] if the arrays do not form a valid tree
pub fn read(path: &Path, expected_root: Option<&Path>) -> Result<Snapshot, SnapshotError> {
    let (header, parts) = read_parts(path)?;
    if let Some(expected) = expected_root
        && expected != header.root_path
    {
        return Err(SnapshotError::RootMismatch {
            expected: expected.to_path_buf(),
            found: header.root_path,
        });
    }
    let tree = Tree::from_parts(parts)?;
    Ok(Snapshot {
        meta: header.meta,
        tree,
    })
}

/// Decodes a complete snapshot from memory.
///
/// # Errors
///
/// * Same as [`read_parts`], minus I/O errors
pub fn decode(data: &[u8]) -> Result<(SnapshotHeader, TreeParts), SnapshotError> {
    if data.len() < PREFIX_LEN {
        return Err(SnapshotError::NotASnapshot);
    }
    let (version, header_len) = check_prefix(&data[..PREFIX_LEN])?;
    if data.len() < PREFIX_LEN + CHECKSUM_LEN {
        return Err(SnapshotError::Corrupt("truncated file"));
    }
    let (body, sum) = data.split_at(data.len() - CHECKSUM_LEN);
    let stored = u64::from_le_bytes(
        sum.try_into()
            .map_err(|_| SnapshotError::Corrupt("checksum"))?,
    );
    if xxh3_64(body) != stored {
        return Err(SnapshotError::ChecksumMismatch);
    }
    let mut cur = Cursor::new(&body[PREFIX_LEN..]);
    let header = cur.take(header_len as usize)?;
    let (root_path, node_count, meta) = decode_header(&mut Cursor::new(header))?;
    let n = usize::try_from(node_count).map_err(|_| SnapshotError::Corrupt("node count"))?;

    let names = cur.bytes_section(None)?;
    let name_off = cur.u64_section(n)?;
    let name_len = cur.u32_section(n)?;
    let parent = cur.u32_section(n)?;
    let first_child = cur.u32_section(n)?;
    let next_sibling = cur.u32_section(n)?;
    let kind = cur.bytes_section(Some(n))?;
    let flags = cur.bytes_section(Some(n))?;
    let own_apparent = cur.u64_section(n)?;
    let own_allocated = cur.u64_section(n)?;
    let own_items = cur.u64_section(n)?;
    let mtime = cur.i64_section(n)?;
    if !cur.is_empty() {
        return Err(SnapshotError::Corrupt("trailing data"));
    }
    let parts = TreeParts {
        root_path: root_path.clone(),
        names,
        name_off,
        name_len,
        parent,
        first_child,
        next_sibling,
        kind,
        flags,
        own_apparent,
        own_allocated,
        own_items,
        mtime,
    };
    Ok((
        SnapshotHeader {
            version,
            root_path,
            node_count,
            meta,
        },
        parts,
    ))
}

fn check_prefix(prefix: &[u8]) -> Result<(u32, u32), SnapshotError> {
    if prefix[..8] != MAGIC {
        return Err(SnapshotError::NotASnapshot);
    }
    let version = u32::from_le_bytes([prefix[8], prefix[9], prefix[10], prefix[11]]);
    if version != FORMAT_VERSION {
        return Err(SnapshotError::UnsupportedVersion(version));
    }
    let header_len = u32::from_le_bytes([prefix[12], prefix[13], prefix[14], prefix[15]]);
    if header_len > MAX_HEADER_LEN {
        return Err(SnapshotError::Corrupt("header too large"));
    }
    Ok((version, header_len))
}

fn encode_header(parts: &TreeParts, meta: &SnapshotMeta) -> Result<Vec<u8>, SnapshotError> {
    fn put_bytes(out: &mut Vec<u8>, bytes: &[u8], what: &'static str) -> Result<(), SnapshotError> {
        let len = u32::try_from(bytes.len()).map_err(|_| SnapshotError::TooLarge(what))?;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(bytes);
        Ok(())
    }
    let mut out = Vec::with_capacity(128);
    out.extend_from_slice(&meta.scanned_at.to_le_bytes());
    put_bytes(&mut out, meta.backend.as_bytes(), "backend")?;
    put_bytes(
        &mut out,
        &os_to_bytes(parts.root_path.as_os_str()),
        "root path",
    )?;
    match &meta.cursor {
        Some(cursor) => {
            out.push(1);
            put_bytes(&mut out, cursor, "cursor")?;
        }
        None => out.push(0),
    }
    out.extend_from_slice(&(parts.parent.len() as u64).to_le_bytes());
    Ok(out)
}

fn decode_header(cur: &mut Cursor<'_>) -> Result<(PathBuf, u64, SnapshotMeta), SnapshotError> {
    let scanned_at = cur.i64()?;
    let backend = String::from_utf8(cur.len_prefixed()?.to_vec())
        .map_err(|_| SnapshotError::Corrupt("backend is not UTF-8"))?;
    let root_path = PathBuf::from(bytes_to_os(cur.len_prefixed()?));
    let cursor = match cur.u8()? {
        0 => None,
        1 => Some(cur.len_prefixed()?.to_vec()),
        _ => return Err(SnapshotError::Corrupt("cursor flag")),
    };
    let node_count = cur.u64()?;
    Ok((
        root_path,
        node_count,
        SnapshotMeta {
            scanned_at,
            backend,
            cursor,
        },
    ))
}

fn write_sections<W: Write>(w: &mut W, parts: &TreeParts) -> io::Result<()> {
    write_bytes(w, &parts.names)?;
    write_u64s(w, &parts.name_off)?;
    write_u32s(w, &parts.name_len)?;
    write_u32s(w, &parts.parent)?;
    write_u32s(w, &parts.first_child)?;
    write_u32s(w, &parts.next_sibling)?;
    write_bytes(w, &parts.kind)?;
    write_bytes(w, &parts.flags)?;
    write_u64s(w, &parts.own_apparent)?;
    write_u64s(w, &parts.own_allocated)?;
    write_u64s(w, &parts.own_items)?;
    write_i64s(w, &parts.mtime)
}

fn section_header<W: Write>(w: &mut W, width: u8, count: usize) -> io::Result<()> {
    w.write_all(&[width])?;
    w.write_all(&(count as u64).to_le_bytes())
}

fn write_bytes<W: Write>(w: &mut W, data: &[u8]) -> io::Result<()> {
    section_header(w, 1, data.len())?;
    w.write_all(data)
}

fn write_u32s<W: Write>(w: &mut W, data: &[u32]) -> io::Result<()> {
    section_header(w, 4, data.len())?;
    write_chunked(w, data, 4, |v, out| out.extend_from_slice(&v.to_le_bytes()))
}

fn write_i64s<W: Write>(w: &mut W, data: &[i64]) -> io::Result<()> {
    section_header(w, 8, data.len())?;
    write_chunked(w, data, 8, |v, out| out.extend_from_slice(&v.to_le_bytes()))
}

fn write_u64s<W: Write>(w: &mut W, data: &[u64]) -> io::Result<()> {
    let narrow = data.iter().all(|v| u32::try_from(*v).is_ok());
    if narrow {
        section_header(w, 4, data.len())?;
        #[allow(clippy::cast_possible_truncation)] // every value was checked to fit in u32
        write_chunked(w, data, 4, |v, out| {
            out.extend_from_slice(&(*v as u32).to_le_bytes());
        })
    } else {
        section_header(w, 8, data.len())?;
        write_chunked(w, data, 8, |v, out| out.extend_from_slice(&v.to_le_bytes()))
    }
}

fn write_chunked<W: Write, T>(
    w: &mut W,
    data: &[T],
    width: usize,
    encode: impl Fn(&T, &mut Vec<u8>),
) -> io::Result<()> {
    let mut buf = Vec::with_capacity(CHUNK * width);
    for chunk in data.chunks(CHUNK) {
        buf.clear();
        for v in chunk {
            encode(v, &mut buf);
        }
        w.write_all(&buf)?;
    }
    Ok(())
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(format!(".tmp-{}", std::process::id()));
    path.with_file_name(name)
}

/// Best-effort fsync of the containing directory so the rename itself is durable.
#[cfg(unix)]
fn sync_parent(path: &Path) {
    if let Some(dir) = path.parent()
        && let Ok(dir) = File::open(dir)
    {
        let _ = dir.sync_all();
    }
}

/// Directory fsync is not available on this platform; `MoveFileEx` rename is already durable
/// enough for a cache file that is validated on load.
#[cfg(not(unix))]
const fn sync_parent(_path: &Path) {}

/// Writer that hashes everything it forwards and counts bytes.
struct HashWriter<W> {
    inner: W,
    hasher: Xxh3,
    written: u64,
}

impl<W: Write> HashWriter<W> {
    const fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Xxh3::new(),
            written: 0,
        }
    }
}

impl<W: Write> Write for HashWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Bounds-checked little-endian reader over a byte slice.
struct Cursor<'a> {
    buf: &'a [u8],
}

impl<'a> Cursor<'a> {
    const fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    const fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    const fn take(&mut self, n: usize) -> Result<&'a [u8], SnapshotError> {
        if n > self.buf.len() {
            return Err(SnapshotError::Corrupt("truncated data"));
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], SnapshotError> {
        self.take(N)?
            .try_into()
            .map_err(|_| SnapshotError::Corrupt("truncated data"))
    }

    fn u8(&mut self) -> Result<u8, SnapshotError> {
        Ok(self.array::<1>()?[0])
    }

    fn u32(&mut self) -> Result<u32, SnapshotError> {
        self.array().map(u32::from_le_bytes)
    }

    fn u64(&mut self) -> Result<u64, SnapshotError> {
        self.array().map(u64::from_le_bytes)
    }

    fn i64(&mut self) -> Result<i64, SnapshotError> {
        self.array().map(i64::from_le_bytes)
    }

    fn len_prefixed(&mut self) -> Result<&'a [u8], SnapshotError> {
        let len = self.u32()?;
        self.take(len as usize)
    }

    /// Reads a section header and returns `(width, data)`.
    fn section(&mut self, expected_count: Option<usize>) -> Result<(u8, &'a [u8]), SnapshotError> {
        let width = self.u8()?;
        if !matches!(width, 1 | 4 | 8) {
            return Err(SnapshotError::Corrupt("section width"));
        }
        let count =
            usize::try_from(self.u64()?).map_err(|_| SnapshotError::Corrupt("section count"))?;
        if expected_count.is_some_and(|n| n != count) {
            return Err(SnapshotError::Corrupt("section length mismatch"));
        }
        let len = count
            .checked_mul(usize::from(width))
            .ok_or(SnapshotError::Corrupt("section size"))?;
        Ok((width, self.take(len)?))
    }

    fn bytes_section(&mut self, expected_count: Option<usize>) -> Result<Vec<u8>, SnapshotError> {
        match self.section(expected_count)? {
            (1, data) => Ok(data.to_vec()),
            _ => Err(SnapshotError::Corrupt("byte section width")),
        }
    }

    fn u32_section(&mut self, n: usize) -> Result<Vec<u32>, SnapshotError> {
        match self.section(Some(n))? {
            (4, data) => Ok(data
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| u32::from_le_bytes(*c))
                .collect()),
            _ => Err(SnapshotError::Corrupt("u32 section width")),
        }
    }

    fn u64_section(&mut self, n: usize) -> Result<Vec<u64>, SnapshotError> {
        match self.section(Some(n))? {
            (4, data) => Ok(data
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| u64::from(u32::from_le_bytes(*c)))
                .collect()),
            (8, data) => Ok(data
                .as_chunks::<8>()
                .0
                .iter()
                .map(|c| u64::from_le_bytes(*c))
                .collect()),
            _ => Err(SnapshotError::Corrupt("u64 section width")),
        }
    }

    fn i64_section(&mut self, n: usize) -> Result<Vec<i64>, SnapshotError> {
        match self.section(Some(n))? {
            (8, data) => Ok(data
                .as_chunks::<8>()
                .0
                .iter()
                .map(|c| i64::from_le_bytes(*c))
                .collect()),
            _ => Err(SnapshotError::Corrupt("i64 section width")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bclean_tree::{EntryKind, EntryMeta, NodeId, SizeMode};
    use std::ffi::OsStr;

    fn sample_tree() -> Tree {
        let mut tree = Tree::new("/data/root", EntryMeta::dir());
        let a = tree.add_child_raw(NodeId::ROOT, OsStr::new("a"), EntryMeta::dir());
        tree.add_child_raw(
            a,
            OsStr::new("big"),
            EntryMeta::new(EntryKind::File, 5 << 32, 5 << 32, 7),
        );
        tree.add_child_raw(
            a,
            OsStr::new("small"),
            EntryMeta::new(EntryKind::File, 10, 4096, -3),
        );
        tree.add_child_raw(
            NodeId::ROOT,
            OsStr::new("ü"),
            EntryMeta::new(EntryKind::Symlink, 1, 0, 9),
        );
        tree.recompute_aggregates();
        tree
    }

    fn meta() -> SnapshotMeta {
        SnapshotMeta {
            scanned_at: 1_700_000_000,
            backend: "NATIVE".into(),
            cursor: Some(b"{\"x\":1}".to_vec()),
        }
    }

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.snap");
        let tree = sample_tree();
        let written = write_tree(&path, &tree, &meta()).unwrap();
        assert_eq!(written, fs::metadata(&path).unwrap().len());

        let header = read_header(&path).unwrap();
        assert_eq!(header.root_path, PathBuf::from("/data/root"));
        assert_eq!(header.node_count, 5);
        assert_eq!(header.meta, meta());

        let snap = read(&path, Some(Path::new("/data/root"))).unwrap();
        assert_eq!(snap.meta, meta());
        assert_eq!(snap.tree.clone().into_parts(), tree.clone().into_parts());
        assert_eq!(
            snap.tree.size(NodeId::ROOT, SizeMode::Apparent),
            tree.size(NodeId::ROOT, SizeMode::Apparent)
        );
        assert!(snap.tree.find(Path::new("/data/root/a/big")).is_some());
        // No temp files left behind.
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn rejects_mismatched_root() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.snap");
        write_tree(&path, &sample_tree(), &SnapshotMeta::default()).unwrap();
        let err = read(&path, Some(Path::new("/other"))).unwrap_err();
        assert!(matches!(err, SnapshotError::RootMismatch { .. }));
        assert!(err.is_invalid());
    }

    #[test]
    fn rejects_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.snap");
        write_tree(&path, &sample_tree(), &meta()).unwrap();
        let good = fs::read(&path).unwrap();

        // Flipping any single byte past the prefix must be caught by the checksum.
        for i in PREFIX_LEN..good.len() {
            let mut bad = good.clone();
            bad[i] ^= 0x5a;
            let err = decode(&bad).unwrap_err();
            assert!(err.is_invalid(), "byte {i}: {err}");
        }
        // Truncation at any point is rejected.
        for len in 0..good.len() {
            assert!(decode(&good[..len]).is_err(), "len {len}");
        }
        let mut bad = good.clone();
        bad[0] = b'X';
        assert!(matches!(decode(&bad), Err(SnapshotError::NotASnapshot)));
        let mut bad = good;
        bad[8] = 99;
        assert!(matches!(
            decode(&bad),
            Err(SnapshotError::UnsupportedVersion(99))
        ));
        assert!(matches!(
            read(&dir.path().join("missing"), None),
            Err(SnapshotError::Io(_))
        ));
    }

    #[test]
    fn checksum_valid_but_bad_links_rejected() {
        let mut parts = sample_tree().into_parts();
        parts.parent[1] = 77;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.snap");
        write_parts(&path, &parts, &meta()).unwrap();
        assert!(matches!(read(&path, None), Err(SnapshotError::Parts(_))));
    }

    /// Synthetic save/load benchmark. Run with:
    /// `cargo test -p bclean_index --release -- --ignored --nocapture bench_snapshot`
    #[test]
    #[ignore = "benchmark; run explicitly in release mode"]
    fn bench_snapshot_5m() {
        use std::time::Instant;
        const DIRS: usize = 250_000;
        const FILES_PER_DIR: usize = 19;
        let t = Instant::now();
        let mut tree = Tree::with_capacity(
            "/bench",
            EntryMeta::dir(),
            DIRS * (FILES_PER_DIR + 1) + 1,
            0,
        );
        let mut dirs = vec![NodeId::ROOT];
        for d in 0..DIRS {
            let parent = dirs[d / 8];
            let dir =
                tree.add_child_raw(parent, OsStr::new(&format!("dir-{d:07}")), EntryMeta::dir());
            dirs.push(dir);
            for f in 0..FILES_PER_DIR {
                let size = ((d * 31 + f * 7) % 100_000) as u64;
                tree.add_child_raw(
                    dir,
                    OsStr::new(&format!("file-{f:03}.dat")),
                    EntryMeta::new(
                        EntryKind::File,
                        size,
                        size.div_ceil(4096) * 4096,
                        1_700_000_000,
                    ),
                );
            }
        }
        tree.recompute_aggregates();
        let nodes = tree.len();
        eprintln!("built {nodes} nodes in {:?}", t.elapsed());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bench.snap");
        let t = Instant::now();
        let parts = tree.into_parts();
        let into = t.elapsed();
        let t = Instant::now();
        let bytes = write_parts(&path, &parts, &meta()).unwrap();
        let save = t.elapsed();
        drop(parts);
        let t = Instant::now();
        let data = fs::read(&path).unwrap();
        let read_t = t.elapsed();
        let t = Instant::now();
        let (_, parts) = decode(&data).unwrap();
        let decode_t = t.elapsed();
        drop(data);
        let t = Instant::now();
        let tree = Tree::from_parts(parts).unwrap();
        let rebuild = t.elapsed();
        assert_eq!(tree.len(), nodes);
        eprintln!(
            "nodes={nodes} file={:.1} MiB into_parts={into:?} save(write+fsync+rename)={save:?} \
             read={read_t:?} decode(verify+convert)={decode_t:?} from_parts={rebuild:?}",
            f64::from(u32::try_from(bytes >> 10).unwrap_or(u32::MAX)) / 1024.0
        );
    }
}
