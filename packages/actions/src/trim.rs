//! Trimming: deleting a directory's contents except the files worth keeping
//! ([`ActionSpec::Trim`](bclean_model::ActionSpec::Trim)).
//!
//! A Cargo `target/` trimmed with the Rust defaults keeps `release/bcode` (and the plugin
//! dylibs next to it) while everything that only serves rebuilding goes: `deps/`, `build/`,
//! `incremental/`, `.fingerprint/`, the debug profile. Kept files stay where they are, so the
//! binary keeps working from its usual path. Cargo hard-links `release/bcode` to a file under
//! `deps/`; removing that link leaves the kept one intact.
//!
//! A directory matched by a pattern without the `executable` condition (`dev-db/**` keeps
//! `dev-db`) is kept whole, empty subdirectories included, and never descended into.
//!
//! Symlinks are never followed: a link is kept or removed as an entry of its own.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::Metadata;
use std::io;
use std::path::{Path, PathBuf};

use bclean_model::KeepSpec;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::CancelToken;

/// Compiled [`KeepSpec`]s.
#[derive(Debug, Clone)]
pub struct Keeper {
    set: GlobSet,
    executable: Vec<bool>,
}

impl Keeper {
    /// Compiles `keep`.
    ///
    /// # Errors
    ///
    /// * If a pattern is not a valid glob
    pub fn new(keep: &[KeepSpec]) -> Result<Self, globset::Error> {
        let mut set = GlobSetBuilder::new();
        for spec in keep {
            set.add(
                GlobBuilder::new(&spec.glob)
                    .literal_separator(true)
                    .case_insensitive(cfg!(windows))
                    .build()?,
            );
        }
        Ok(Self {
            set: set.build()?,
            executable: keep.iter().map(|k| k.executable).collect(),
        })
    }

    /// Whether the directory at `rel` (relative to the trimmed directory) is kept whole: it,
    /// or a directory above it, matches a pattern without the `executable` condition.
    #[must_use]
    pub fn keeps_tree(&self, rel: &Path) -> bool {
        rel.ancestors()
            .filter(|dir| !dir.as_os_str().is_empty())
            .any(|dir| {
                self.set
                    .matches(dir)
                    .into_iter()
                    .any(|i| !self.executable[i])
            })
    }

    /// Whether the file at `rel` (relative to the trimmed directory) is kept.
    #[must_use]
    pub fn keeps(&self, rel: &Path, meta: &Metadata) -> bool {
        self.set
            .matches(rel)
            .into_iter()
            .any(|i| !self.executable[i] || is_executable(rel, meta))
    }
}

/// What a trim did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Trimmed {
    /// Kept files, relative to the trimmed directory.
    pub kept: Vec<PathBuf>,
    /// Bytes of the kept files.
    pub kept_bytes: u64,
    /// Bytes of removed files whose data no other link still holds.
    pub freed: u64,
}

/// Deletes everything inside `dir` except the files `keeper` keeps, and the directories that
/// end up empty (`dir` itself stays). `removed` is called with the bytes freed so far.
///
/// # Errors
///
/// * If `dir` cannot be read or an entry cannot be removed
/// * [`io::ErrorKind::Interrupted`] if `cancel` is raised (entries removed so far stay removed)
pub fn trim(
    dir: &Path,
    keeper: &Keeper,
    cancel: &CancelToken,
    removed: &mut dyn FnMut(u64),
) -> io::Result<Trimmed> {
    let mut out = Trimmed::default();
    trim_dir(dir, Path::new(""), keeper, cancel, removed, &mut out)?;
    Ok(out)
}

/// Trims `dir` (at `rel` inside the trimmed directory); returns whether anything remains in it.
fn trim_dir(
    dir: &Path,
    rel: &Path,
    keeper: &Keeper,
    cancel: &CancelToken,
    removed: &mut dyn FnMut(u64),
    out: &mut Trimmed,
) -> io::Result<bool> {
    let mut remains = false;
    for entry in std::fs::read_dir(dir)? {
        if cancel.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let entry = entry?;
        let path = entry.path();
        let rel = rel.join(entry.file_name());
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.is_dir() && keeper.keeps_tree(&rel) {
            // Kept whole, empty directories included: a database's data directory needs them.
            out.kept_bytes += tree_bytes(&path);
            out.kept.push(rel);
            remains = true;
        } else if meta.is_dir() {
            if trim_dir(&path, &rel, keeper, cancel, removed, out)? {
                remains = true;
            } else {
                std::fs::remove_dir(&path)?;
            }
        } else if keeper.keeps(&rel, &meta) {
            out.kept_bytes += meta.len();
            out.kept.push(rel);
            remains = true;
        } else {
            std::fs::remove_file(&path)?;
            if last_link(&meta) {
                out.freed += meta.len();
                removed(out.freed);
            }
        }
    }
    Ok(remains)
}

/// Bounds for [`trimmable`].
#[derive(Debug, Clone, Copy)]
pub struct TrimProbe {
    /// Stop once this many bytes are known to be removable.
    pub enough: u64,
    /// Stop after visiting this many entries.
    pub max_entries: usize,
    /// Count allocated (on-disk) bytes instead of file lengths.
    pub allocated: bool,
}

impl Default for TrimProbe {
    fn default() -> Self {
        Self {
            enough: 64 << 20,
            max_entries: 20_000,
            allocated: true,
        }
    }
}

/// Bytes a [`trim`] of `dir` would free, if fewer than `probe.enough`.
///
/// `None` when there is at least that much, when `probe.max_entries` entries were not enough
/// to tell, or when the directory cannot be read.
///
/// A trimmed directory holds only what its trim keeps, so it is listed in full in a few
/// entries and reports what little a second trim would free (usually nothing). An untrimmed
/// one reaches `enough` within its first few build products. Removed files whose data a kept
/// file still holds (Cargo hard-links `release/bcode` into `deps/`) free nothing and are
/// not counted.
#[must_use]
pub fn trimmable(dir: &Path, keeper: &Keeper, probe: TrimProbe) -> Option<u64> {
    let mut removable: BTreeMap<FileId, u64> = BTreeMap::new();
    let mut kept: BTreeSet<FileId> = BTreeSet::new();
    let mut total = 0_u64;
    let mut visited = 0_usize;
    let mut stack = vec![(dir.to_path_buf(), PathBuf::new())];
    while let Some((current, rel_dir)) = stack.pop() {
        for entry in std::fs::read_dir(&current).ok()? {
            visited += 1;
            if visited > probe.max_entries {
                return None;
            }
            let entry = entry.ok()?;
            let rel = rel_dir.join(entry.file_name());
            let meta = entry.path().symlink_metadata().ok()?;
            if meta.is_dir() {
                if !keeper.keeps_tree(&rel) {
                    stack.push((entry.path(), rel));
                }
            } else if keeper.keeps(&rel, &meta) {
                if let Some(id) = file_id(&meta) {
                    total = total.saturating_sub(removable.remove(&id).unwrap_or(0));
                    kept.insert(id);
                }
            } else {
                let bytes = if probe.allocated {
                    allocated_bytes(&meta)
                } else {
                    meta.len()
                };
                if let Some(id) = file_id(&meta)
                    && (kept.contains(&id) || removable.insert(id, bytes).is_some())
                {
                    continue;
                }
                total += bytes;
                if total >= probe.enough {
                    return None;
                }
            }
        }
    }
    Some(total)
}

/// Identity of a file with more than one link, so its data is counted once (`None`: a file
/// with a single link, or no way to tell).
type FileId = (u64, u64);

#[cfg(unix)]
fn file_id(meta: &Metadata) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt as _;
    (meta.nlink() > 1).then(|| (meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
const fn file_id(_meta: &Metadata) -> Option<FileId> {
    None
}

#[cfg(unix)]
fn allocated_bytes(meta: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    meta.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
fn allocated_bytes(meta: &Metadata) -> u64 {
    meta.len()
}

/// Bytes of the files under `dir` (unreadable entries count as empty).
fn tree_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.path().symlink_metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

#[cfg(unix)]
fn is_executable(_rel: &Path, meta: &Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    meta.is_file() && meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(rel: &Path, meta: &Metadata) -> bool {
    meta.is_file()
        && rel
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
}

/// Whether removing this entry frees its data (no other hard link holds it).
#[cfg(unix)]
fn last_link(meta: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    meta.nlink() <= 1
}

#[cfg(not(unix))]
const fn last_link(_meta: &Metadata) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, bytes: usize, executable: bool) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![0_u8; bytes]).unwrap();
        #[cfg(unix)]
        if executable {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        #[cfg(not(unix))]
        let _ = executable;
    }

    fn rust_defaults() -> Vec<KeepSpec> {
        ["release/*", "*/release/*"]
            .into_iter()
            .flat_map(|dir| {
                [
                    KeepSpec {
                        glob: dir.to_owned(),
                        executable: true,
                    },
                    KeepSpec {
                        glob: format!("{dir}.{{dylib,so,dll}}"),
                        executable: false,
                    },
                ]
            })
            .collect()
    }

    #[test]
    fn keeps_release_binaries_and_dylibs_only() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let exe = if cfg!(windows) { "bcode.exe" } else { "bcode" };
        write(&target.join("release/deps").join(exe), 100, true);
        #[cfg(unix)]
        std::fs::hard_link(
            target.join("release/deps").join(exe),
            target.join("release").join(exe),
        )
        .unwrap();
        #[cfg(not(unix))]
        write(&target.join("release").join(exe), 100, true);
        write(&target.join("release/libplugin.dylib"), 50, false);
        write(&target.join("release/bcode.d"), 5, false);
        write(&target.join("release/build/x/out/gen.rs"), 5, false);
        write(&target.join("release/.fingerprint/x/hash"), 5, false);
        write(&target.join("debug").join(exe), 100, true);
        write(
            &target.join("aarch64-apple-darwin/release").join(exe),
            30,
            true,
        );
        write(&target.join(".rustc_info.json"), 5, false);

        let keeper = Keeper::new(&rust_defaults()).unwrap();
        let trimmed = trim(&target, &keeper, &CancelToken::new(), &mut |_| {}).unwrap();

        let mut kept: Vec<String> = trimmed
            .kept
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        kept.sort();
        assert_eq!(
            kept,
            [
                format!("aarch64-apple-darwin/release/{exe}"),
                format!("release/{exe}"),
                "release/libplugin.dylib".to_owned(),
            ]
        );
        assert_eq!(
            std::fs::read(target.join("release").join(exe))
                .unwrap()
                .len(),
            100
        );
        assert!(!target.join("release/deps").exists());
        assert!(!target.join("release/build").exists());
        assert!(!target.join("debug").exists());
        assert!(!target.join(".rustc_info.json").exists());
        assert!(target.is_dir());
        // The hard-linked deps copy shared its data with the kept binary: not freed.
        assert_eq!(
            trimmed.freed,
            if cfg!(unix) { 5 * 4 + 100 } else { 5 * 4 + 200 }
        );
    }

    #[test]
    fn trimmable_reports_what_a_trim_would_free() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        write(&target.join("release/deps/bcode-1234"), 4096, true);
        #[cfg(unix)]
        std::fs::hard_link(
            target.join("release/deps/bcode-1234"),
            target.join("release/bcode"),
        )
        .unwrap();
        #[cfg(not(unix))]
        write(&target.join("release/bcode.exe"), 4096, true);
        write(&target.join("debug/deps/libx.rlib"), 8192, false);
        let keeper = Keeper::new(&rust_defaults()).unwrap();
        let probe = TrimProbe {
            allocated: false,
            ..TrimProbe::default()
        };
        // The deps copy of the binary shares its data with the kept one.
        let expected = if cfg!(unix) { 8192 } else { 8192 + 4096 };
        assert_eq!(trimmable(&target, &keeper, probe), Some(expected));
        let small = TrimProbe {
            enough: 1024,
            ..probe
        };
        assert_eq!(trimmable(&target, &keeper, small), None, "enough to clean");

        trim(&target, &keeper, &CancelToken::new(), &mut |_| {}).unwrap();
        assert_eq!(
            trimmable(&target, &keeper, probe),
            Some(0),
            "trimmed already"
        );
        assert_eq!(trimmable(&target, &keeper, small), Some(0));
    }

    #[test]
    fn executable_patterns_skip_plain_files() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("release/notes.txt"), 1, false);
        let keeper = Keeper::new(&[KeepSpec {
            glob: "release/*".to_owned(),
            executable: true,
        }])
        .unwrap();
        let trimmed = trim(dir.path(), &keeper, &CancelToken::new(), &mut |_| {}).unwrap();
        assert_eq!(trimmed.kept, Vec::<PathBuf>::new());
        assert!(!dir.path().join("release").exists());
    }
}
