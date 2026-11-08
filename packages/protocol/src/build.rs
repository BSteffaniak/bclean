//! Fingerprint of the running executable.
//!
//! Development builds all report the same package version, so the version alone cannot tell
//! a daemon started from an older build apart from a current one. Clients compare this
//! fingerprint in the handshake and restart a daemon that runs different code.

use std::hash::{DefaultHasher, Hasher};
use std::sync::OnceLock;

/// A hash of the running executable's contents, computed once per process (`None` if the
/// executable cannot be read).
#[must_use]
pub fn executable_build_id() -> Option<String> {
    static ID: OnceLock<Option<String>> = OnceLock::new();
    ID.get_or_init(installed_build_id).clone()
}

/// A hash of the executable file at this process's path as it is now, which differs from
/// [`executable_build_id`] once a rebuild replaced it (`None` if it cannot be read).
#[must_use]
pub fn installed_build_id() -> Option<String> {
    let bytes = std::fs::read(std::env::current_exe().ok()?).ok()?;
    let mut hasher = DefaultHasher::new();
    hasher.write(&bytes);
    Some(format!("{:016x}", hasher.finish()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_id_is_stable_within_a_process() {
        let id = executable_build_id().expect("the test binary is readable");
        assert_eq!(id.len(), 16);
        assert_eq!(executable_build_id().as_deref(), Some(id.as_str()));
        assert_eq!(installed_build_id(), Some(id));
    }
}
