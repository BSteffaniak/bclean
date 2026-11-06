//! `OS_LAST_USED`: operating-system "last used" metadata.
//!
//! * macOS: Spotlight `kMDItemLastUsedDate` through the `CoreServices` `MDItem` API (see the
//!   platform module for why FFI was preferred over spawning `mdls`).
//! * Windows: the NTFS last access time of the item itself.
//! * Linux and others: no OS-level last-used metadata exists; the provider reports nothing
//!   (file access times are handled by `FS_TIMES` when trustworthy).

use std::path::Path;

use bclean_model::{SignalKind, SignalReading};

use crate::{SignalProvider, Target, platform};

/// OS last-used time for `path` (Unix seconds) and a description of its source.
#[must_use]
pub fn os_last_used(path: &Path) -> Option<(i64, &'static str)> {
    platform::os_last_used(path)
}

/// `OS_LAST_USED` provider.
#[derive(Debug, Clone, Copy, Default)]
pub struct OsLastUsedProvider;

impl SignalProvider for OsLastUsedProvider {
    fn kind(&self) -> SignalKind {
        SignalKind::OsLastUsed
    }

    fn read(&self, target: &Target<'_>) -> Option<SignalReading> {
        let (time, source) = os_last_used(target.path)?;
        Some(SignalReading {
            kind: SignalKind::OsLastUsed,
            last_activity: Some(time),
            forces_stale: false,
            detail: source.to_owned(),
        })
    }
}
