//! Linux volumes via `/proc/self/mountinfo` + `statvfs(3)`.
//!
//! Pseudo filesystems (`proc`, `sysfs`, `cgroup*`, `devpts`, ...) are always skipped. RAM-backed
//! filesystems (`tmpfs`, `ramfs`), network filesystems, read-only mounts (e.g. snap
//! `squashfs` images), and container `overlay` mounts other than `/` are opt-in through
//! [`VolumeFilter`]. Bind mounts and repeated mounts of one device are reported once, under
//! the shortest mount point.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use bclean_model::VolumeStats;

use super::{VolumeError, VolumeFilter, now};

const PSEUDO_FS: &[&str] = &[
    "proc",
    "sysfs",
    "cgroup",
    "cgroup2",
    "devtmpfs",
    "devpts",
    "securityfs",
    "pstore",
    "debugfs",
    "tracefs",
    "configfs",
    "fusectl",
    "mqueue",
    "hugetlbfs",
    "bpf",
    "autofs",
    "binfmt_misc",
    "rpc_pipefs",
    "nsfs",
    "efivarfs",
    "selinuxfs",
    "nfsd",
    "fuse.gvfsd-fuse",
    "fuse.portal",
    "fuse.lxcfs",
    "fuse.snapfuse",
];
const RAM_FS: &[&str] = &["tmpfs", "ramfs"];
const REMOTE_FS: &[&str] = &[
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "smbfs",
    "9p",
    "afs",
    "ceph",
    "glusterfs",
    "fuse.sshfs",
    "fuse.rclone",
    "davfs",
    "fuse.glusterfs",
    "lustre",
];

/// One parsed `mountinfo` line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mount {
    dev: (u32, u32),
    path: String,
    read_only: bool,
    fs_type: String,
}

pub fn list(filter: VolumeFilter) -> Result<Vec<VolumeStats>, VolumeError> {
    let mounts = parse_mountinfo(&fs::read_to_string("/proc/self/mountinfo")?);
    let mut by_dev: BTreeMap<(u32, u32), Mount> = BTreeMap::new();
    for m in mounts.into_iter().filter(|m| keep(m, filter)) {
        match by_dev.get(&m.dev) {
            Some(existing) if existing.path.len() <= m.path.len() => {}
            _ => {
                by_dev.insert(m.dev, m);
            }
        }
    }
    let at = now();
    let mut out = Vec::with_capacity(by_dev.len());
    for m in by_dev.into_values() {
        // Mounts we cannot stat (permissions, stale network mounts) are skipped.
        if let Ok(stats) = statvfs(Path::new(&m.path), m.path.clone(), m.fs_type, at)
            && (stats.total > 0 || filter.include_empty)
        {
            out.push(stats);
        }
    }
    Ok(out)
}

pub fn volume_for(path: &Path) -> Result<VolumeStats, VolumeError> {
    let meta = fs::metadata(path)?;
    let dev = (libc::major(meta.dev()), libc::minor(meta.dev()));
    let canonical = fs::canonicalize(path)?;
    let mounts = parse_mountinfo(&fs::read_to_string("/proc/self/mountinfo")?);
    let contains = |m: &Mount| canonical.starts_with(&m.path);
    let best = mounts
        .iter()
        .filter(|m| m.dev == dev && contains(m))
        .max_by_key(|m| m.path.len())
        .or_else(|| {
            mounts
                .iter()
                .filter(|m| contains(m))
                .max_by_key(|m| m.path.len())
        });
    let (mount_point, fs_type) = best.map_or_else(
        || ("/".to_string(), String::new()),
        |m| (m.path.clone(), m.fs_type.clone()),
    );
    Ok(statvfs(path, mount_point, fs_type, now())?)
}

fn keep(m: &Mount, filter: VolumeFilter) -> bool {
    let t = m.fs_type.as_str();
    if PSEUDO_FS.contains(&t) {
        return false;
    }
    if RAM_FS.contains(&t) && !filter.include_tmpfs {
        return false;
    }
    if REMOTE_FS.contains(&t) && !filter.include_remote {
        return false;
    }
    if t == "overlay" && m.path != "/" {
        return false;
    }
    !m.read_only || filter.include_read_only
}

fn parse_mountinfo(text: &str) -> Vec<Mount> {
    text.lines().filter_map(parse_line).collect()
}

/// Parses `36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw,errors=continue`.
fn parse_line(line: &str) -> Option<Mount> {
    let (left, right) = line.split_once(" - ")?;
    let mut l = left.split(' ');
    let _id = l.next()?;
    let _parent = l.next()?;
    let (major, minor) = l.next()?.split_once(':')?;
    let _root = l.next()?;
    let mount_point = unescape(l.next()?);
    let opts = l.next()?;
    let fs_type = right.split(' ').next()?.to_string();
    Some(Mount {
        dev: (major.parse().ok()?, minor.parse().ok()?),
        path: mount_point,
        read_only: opts.split(',').any(|o| o == "ro"),
        fs_type,
    })
}

/// Decodes the octal escapes (`\040` etc.) used in `mountinfo` paths.
fn unescape(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && let Some(v) = std::str::from_utf8(&bytes[i + 1..i + 4])
                .ok()
                .and_then(|o| u8::from_str_radix(o, 8).ok())
        {
            out.push(v);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn statvfs(path: &Path, mount_point: String, fs_type: String, at: i64) -> io::Result<VolumeStats> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    // SAFETY: an all-zero statvfs is a valid value (plain integers).
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is NUL-terminated and `st` is a valid, writable statvfs.
    if unsafe { libc::statvfs(c_path.as_ptr(), &raw mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let frsize = widen(st.f_frsize);
    Ok(VolumeStats {
        mount_point,
        fs_type,
        total: widen(st.f_blocks).saturating_mul(frsize),
        free: widen(st.f_bfree).saturating_mul(frsize),
        available: widen(st.f_bavail).saturating_mul(frsize),
        purgeable: None,
        at,
    })
}

/// `statvfs` field widths differ between targets (32 or 64 bits).
#[allow(clippy::useless_conversion)] // identity on 64-bit targets, widening on 32-bit ones
fn widen<T: Into<u64>>(v: T) -> u64 {
    v.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mountinfo() {
        let text = "\
22 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw
23 22 0:21 / /proc rw,nosuid shared:12 - proc proc rw
24 22 0:5 / /dev/shm rw shared:2 - tmpfs tmpfs rw
25 22 259:2 /home /srv/my\\040home rw - ext4 /dev/nvme0n1p2 rw
26 22 7:1 / /snap/core/1 ro,nodev - squashfs /dev/loop1 ro
27 22 0:50 / /var/lib/docker/overlay2/x/merged rw - overlay overlay rw
";
        let mounts = parse_mountinfo(text);
        assert_eq!(mounts.len(), 6);
        assert_eq!(mounts[3].path, "/srv/my home");
        assert!(mounts[4].read_only);
        let kept: Vec<_> = mounts
            .iter()
            .filter(|m| keep(m, VolumeFilter::default()))
            .map(|m| m.path.as_str())
            .collect();
        assert_eq!(kept, vec!["/", "/srv/my home"]);
        let with_tmp = VolumeFilter {
            include_tmpfs: true,
            ..VolumeFilter::default()
        };
        assert!(
            mounts
                .iter()
                .any(|m| m.fs_type == "tmpfs" && keep(m, with_tmp))
        );
    }
}
