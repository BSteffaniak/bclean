//! Linux: detect `noatime` mounts via `/proc/self/mountinfo`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// (mount point, atime maintained) pairs, parsed once.
fn mounts() -> &'static [(PathBuf, bool)] {
    static MOUNTS: OnceLock<Vec<(PathBuf, bool)>> = OnceLock::new();
    MOUNTS.get_or_init(|| {
        std::fs::read_to_string("/proc/self/mountinfo")
            .map(|text| parse_mountinfo(&text))
            .unwrap_or_default()
    })
}

/// Parses mountinfo lines: `id parent major:minor root mount_point options ...`.
pub fn parse_mountinfo(text: &str) -> Vec<(PathBuf, bool)> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split(' ');
            let mount_point = fields.nth(4)?;
            let options = fields.next()?;
            let trusted = !options.split(',').any(|o| o == "noatime");
            Some((PathBuf::from(unescape(mount_point)), trusted))
        })
        .collect()
}

/// Decodes the octal escapes (`\040` = space) used in mountinfo.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1..i + 4].iter().all(u8::is_ascii_digit)
        {
            let value =
                (bytes[i + 1] - b'0') * 64 + (bytes[i + 2] - b'0') * 8 + (bytes[i + 3] - b'0');
            out.push(value);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn atime_trusted(path: &Path) -> bool {
    mounts()
        .iter()
        .filter(|(mount, _)| path.starts_with(mount))
        .max_by_key(|(mount, _)| mount.as_os_str().len())
        .is_some_and(|(_, trusted)| *trusted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_noatime() {
        let text = "22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw\n\
                    30 22 8:2 / /mnt/my\\040disk rw,noatime shared:2 - ext4 /dev/sda2 rw\n";
        let mounts = parse_mountinfo(text);
        assert_eq!(mounts[0], (PathBuf::from("/"), true));
        assert_eq!(mounts[1], (PathBuf::from("/mnt/my disk"), false));
    }
}
