//! Windows: NTFS last access time.
//!
//! `std::fs::Metadata::accessed` returns `ftLastAccessTime`. NTFS may have last-access updates
//! disabled (`fsutil behavior query disablelastaccess`) or batched (up to one hour), so the value
//! can be older than the real last use; that only makes items look staler when updates are
//! disabled, which is why `OS_LAST_USED` is combined with other signals rather than used alone.

use std::path::Path;

use crate::system_time_secs;

pub fn last_access(path: &Path) -> Option<i64> {
    let meta = std::fs::metadata(path).ok()?;
    let secs = system_time_secs(meta.accessed().ok()?);
    (secs > 0).then_some(secs)
}
