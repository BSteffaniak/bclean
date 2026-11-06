//! macOS: `noatime` detection via `statfs` and Spotlight `kMDItemLastUsedDate` via the
//! `CoreServices` `MDItem` C API.
//!
//! The `MDItem` FFI was chosen over spawning `mdls -raw -name kMDItemLastUsedDate` because the
//! classifier may query thousands of paths, and a process spawn per path costs milliseconds;
//! the C API needs only a handful of CoreFoundation calls. Spotlight only records a last-used
//! date for items opened through Launch Services (apps, documents, disk images), so most build
//! directories have none and the provider simply reports nothing for them.

use std::ffi::{CString, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CFIndex = isize;
type CFTypeID = usize;
type MDItemRef = *const c_void;

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
/// Seconds between the Unix epoch and the CoreFoundation epoch (2001-01-01T00:00:00Z).
const CF_ABSOLUTE_TIME_INTERVAL_SINCE_1970: f64 = 978_307_200.0;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithBytes(
        alloc: CFAllocatorRef,
        bytes: *const u8,
        num_bytes: CFIndex,
        encoding: u32,
        is_external_representation: u8,
    ) -> CFStringRef;
    fn CFRelease(cf: CFTypeRef);
    fn CFGetTypeID(cf: CFTypeRef) -> CFTypeID;
    fn CFDateGetTypeID() -> CFTypeID;
    fn CFDateGetAbsoluteTime(date: CFTypeRef) -> f64;
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    static kMDItemLastUsedDate: CFStringRef;
    fn MDItemCreate(allocator: CFAllocatorRef, path: CFStringRef) -> MDItemRef;
    fn MDItemCopyAttribute(item: MDItemRef, name: CFStringRef) -> CFTypeRef;
}

/// Releases a CoreFoundation object on drop.
struct Owned(CFTypeRef);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` is a non-null object returned by a CF "Create"/"Copy" function,
            // which transfers ownership (+1 retain count) to us; it is released exactly once.
            unsafe { CFRelease(self.0) };
        }
    }
}

/// Spotlight's last-used date for `path`, in Unix seconds.
#[allow(clippy::cast_possible_truncation)] // CF dates are well within i64 seconds.
pub fn spotlight_last_used(path: &Path) -> Option<i64> {
    let bytes = path.as_os_str().as_bytes();
    let len = CFIndex::try_from(bytes.len()).ok()?;
    // SAFETY: `bytes` is a valid slice of `len` bytes for the duration of the call; a null
    // allocator selects the default allocator. The returned string is owned by us.
    let cf_path = Owned(unsafe {
        CFStringCreateWithBytes(
            std::ptr::null(),
            bytes.as_ptr(),
            len,
            K_CF_STRING_ENCODING_UTF8,
            0,
        )
    });
    if cf_path.0.is_null() {
        return None;
    }
    // SAFETY: `cf_path.0` is a valid CFString; MDItemCreate returns an owned item or null.
    let item = Owned(unsafe { MDItemCreate(std::ptr::null(), cf_path.0) });
    if item.0.is_null() {
        return None;
    }
    // SAFETY: `item.0` is a valid MDItem and `kMDItemLastUsedDate` is an immutable constant
    // CFString exported by CoreServices. The returned attribute value is owned (Copy rule).
    let value = Owned(unsafe { MDItemCopyAttribute(item.0, kMDItemLastUsedDate) });
    if value.0.is_null() {
        return None;
    }
    // SAFETY: `value.0` is a valid CF object; CFGetTypeID/CFDateGetTypeID have no preconditions.
    let is_date = unsafe { CFGetTypeID(value.0) == CFDateGetTypeID() };
    if !is_date {
        return None;
    }
    // SAFETY: `value.0` was just verified to be a CFDate.
    let absolute = unsafe { CFDateGetAbsoluteTime(value.0) };
    let unix = absolute + CF_ABSOLUTE_TIME_INTERVAL_SINCE_1970;
    (unix.is_finite() && unix > 0.0).then_some(unix as i64)
}

/// Whether the volume containing `path` is mounted without `noatime`.
pub fn atime_trusted(path: &Path) -> bool {
    let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `c_path` is a valid NUL-terminated string and `stats` points to writable memory
    // large enough for a `statfs` struct, which the call fills on success.
    let rc = unsafe { libc::statfs(c_path.as_ptr(), stats.as_mut_ptr()) };
    if rc != 0 {
        return false;
    }
    // SAFETY: `statfs` returned 0, so it fully initialized `stats`.
    let stats = unsafe { stats.assume_init() };
    #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)] // flag bit masks
    let noatime = (stats.f_flags & (libc::MNT_NOATIME as u32)) != 0;
    !noatime
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spotlight_lookup_does_not_crash() {
        // Applications are usually indexed with a last-used date, but CI machines may have
        // Spotlight disabled; only check that the call is well-behaved.
        let _ = spotlight_last_used(Path::new("/System/Applications/Calculator.app"));
        assert_eq!(spotlight_last_used(Path::new("/definitely/not/here")), None);
    }

    #[test]
    fn statfs_works_on_root() {
        let _ = atime_trusted(Path::new("/"));
        assert!(!atime_trusted(Path::new("/definitely/not/here")));
    }
}
