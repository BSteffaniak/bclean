//! `ARCHIVE` (tar + gzip) and `MOVE` primitives.
//!
//! Archives are `.tar.gz` files written with pure-Rust crates (`tar` + `flate2`'s miniz
//! backend) so the crate cross-compiles without a C toolchain. Symlinks are stored as links,
//! never followed.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Component, Path, PathBuf};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

use crate::remove::remove_path;

/// Archives `path` into a new `<name>-<unix time>.tar.gz` file inside `dest_dir` (created if
/// needed) and returns the archive path. The original is left in place.
///
/// # Errors
///
/// * `path` has no file name.
/// * The destination cannot be created or written.
/// * Reading any entry fails.
pub fn archive_to(path: &Path, dest_dir: &Path) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    std::fs::create_dir_all(dest_dir)?;
    let stamp = crate::now_unix();
    let base = name.to_string_lossy();
    let mut archive = dest_dir.join(format!("{base}-{stamp}.tar.gz"));
    let mut n = 1;
    while std::fs::symlink_metadata(&archive).is_ok() {
        archive = dest_dir.join(format!("{base}-{stamp}-{n}.tar.gz"));
        n += 1;
    }
    let partial = archive.with_extension("gz.partial");
    let result = write_archive(path, name.as_ref(), &partial);
    if let Err(err) = result {
        std::fs::remove_file(&partial).ok();
        return Err(err);
    }
    std::fs::rename(&partial, &archive)?;
    Ok(archive)
}

fn write_archive(path: &Path, name: &Path, out: &Path) -> io::Result<()> {
    let file = File::create(out)?;
    let encoder = GzEncoder::new(BufWriter::new(file), Compression::default());
    let mut builder = tar::Builder::new(encoder);
    builder.follow_symlinks(false);
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() {
        builder.append_dir_all(name, path)?;
    } else {
        builder.append_path_with_name(path, name)?;
    }
    let encoder = builder.into_inner()?;
    let mut writer = encoder.finish()?;
    writer.flush()?;
    let file = writer
        .into_inner()
        .map_err(io::IntoInnerError::into_error)?;
    file.sync_all()
}

/// Extracts an archive created by [`archive_to`] into `parent`, recreating `parent/<name>`.
///
/// Every entry must live under the single top-level `name`; entries are unpacked with
/// traversal protection.
///
/// # Errors
///
/// * `parent/<name>` already exists.
/// * The archive is unreadable, or contains entries outside `name`.
pub fn extract(archive: &Path, parent: &Path, name: &Path) -> io::Result<()> {
    let target = parent.join(name);
    if std::fs::symlink_metadata(&target).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already exists", target.display()),
        ));
    }
    std::fs::create_dir_all(parent)?;
    let mut reader = tar::Archive::new(GzDecoder::new(File::open(archive)?));
    reader.set_preserve_mtime(true);
    for entry in reader.entries()? {
        let mut entry = entry?;
        let entry_path = entry.path()?.into_owned();
        let first = entry_path.components().next();
        if first != Some(Component::Normal(name.as_os_str())) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected archive entry {}", entry_path.display()),
            ));
        }
        entry.unpack_in(parent)?;
    }
    Ok(())
}

/// Moves `src` to `dest` (the full destination path, which must not exist).
///
/// Uses `rename`, and falls back to copy + delete when crossing devices. On copy failure the
/// partial copy is removed and `src` is left intact.
///
/// # Errors
///
/// * `dest` already exists.
/// * Renaming fails for a reason other than crossing devices.
/// * The cross-device copy or the subsequent removal of `src` fails.
pub fn move_path(src: &Path, dest: &Path) -> io::Result<()> {
    if std::fs::symlink_metadata(dest).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already exists", dest.display()),
        ));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::CrossesDevices => {
            if let Err(copy_err) = copy_tree(src, dest) {
                if std::fs::symlink_metadata(dest).is_ok() {
                    remove_path(dest).ok();
                }
                return Err(copy_err);
            }
            remove_path(src)
        }
        Err(err) => Err(err),
    }
}

/// Recursively copies `src` to `dst`, recreating symlinks as symlinks.
fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    let file_type = meta.file_type();
    if file_type.is_symlink() {
        let target = std::fs::read_link(src)?;
        return make_symlink(&target, dst, src);
    }
    if file_type.is_dir() {
        std::fs::create_dir(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
        std::fs::set_permissions(dst, meta.permissions())?;
        return Ok(());
    }
    std::fs::copy(src, dst).map(|_| ())
}

#[cfg(unix)]
fn make_symlink(target: &Path, link: &Path, _original: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn make_symlink(target: &Path, link: &Path, original: &Path) -> io::Result<()> {
    let is_dir = std::fs::metadata(original).is_ok_and(|m| m.is_dir());
    if is_dir {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

#[cfg(not(any(unix, windows)))]
fn make_symlink(_target: &Path, _link: &Path, _original: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symlinks unsupported on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("proj");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("sub/file.txt"), b"hello").unwrap();
        let archive = archive_to(&src, &dir.path().join("archives")).unwrap();
        assert!(archive.exists());
        assert!(
            archive
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(".tar.gz")
        );
        remove_path(&src).unwrap();
        extract(&archive, dir.path(), Path::new("proj")).unwrap();
        assert_eq!(std::fs::read(src.join("sub/file.txt")).unwrap(), b"hello");
        // Refuses to overwrite.
        assert!(extract(&archive, dir.path(), Path::new("proj")).is_err());
    }

    #[test]
    fn copy_tree_preserves_content() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a");
        std::fs::create_dir_all(src.join("b")).unwrap();
        std::fs::write(src.join("b/f"), b"1").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("b/f", src.join("l")).unwrap();
        copy_tree(&src, &dir.path().join("c")).unwrap();
        assert_eq!(std::fs::read(dir.path().join("c/b/f")).unwrap(), b"1");
        #[cfg(unix)]
        assert!(
            std::fs::symlink_metadata(dir.path().join("c/l"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}
