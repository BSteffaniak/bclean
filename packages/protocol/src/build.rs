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
    ID.get_or_init(|| {
        let bytes = std::fs::read(std::env::current_exe().ok()?).ok()?;
        let mut hasher = DefaultHasher::new();
        hasher.write(&bytes);
        Some(format!("{:016x}", hasher.finish()))
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_id_is_stable_within_a_process() {
        let id = executable_build_id().expect("the test binary is readable");
        assert_eq!(id.len(), 16);
        assert_eq!(executable_build_id().as_deref(), Some(id.as_str()));
    }
}
