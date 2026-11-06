//! Lossless (Unix) / UTF-8 (other platforms) conversion between paths and bytes.
//!
//! The encoding matches `bclean_tree`'s name arena: raw bytes on Unix, UTF-8 elsewhere with
//! unpaired UTF-16 surrogates replaced.

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};

#[cfg(unix)]
pub fn os_to_bytes(s: &OsStr) -> Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt as _;
    Cow::Borrowed(s.as_bytes())
}

#[cfg(unix)]
pub fn bytes_to_os(bytes: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStrExt as _;
    OsStr::from_bytes(bytes).to_os_string()
}

#[cfg(not(unix))]
pub fn os_to_bytes(s: &OsStr) -> Cow<'_, [u8]> {
    match s.to_string_lossy() {
        Cow::Borrowed(s) => Cow::Borrowed(s.as_bytes()),
        Cow::Owned(s) => Cow::Owned(s.into_bytes()),
    }
}

#[cfg(not(unix))]
pub fn bytes_to_os(bytes: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(bytes).into_owned())
}
