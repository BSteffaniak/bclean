//! Background scheduling priority for the daemon process.
//!
//! The daemon is a background service: its scans should use idle CPU, not compete with
//! foreground work. Installed services get this from launchd / systemd / Task
//! Scheduler; [`lower_process_priority`] applies the same policy to a daemon spawned on demand
//! by a client.

/// Nice value applied on Unix (matches the launchd and systemd service units).
#[cfg(unix)]
const NICE: libc::c_int = 10;

/// Lowers the current process to background CPU priority.
///
/// Disk I/O priority is left alone: deletions requested from the TUI or CLI run in the
/// daemon, and throttling its I/O (macOS `IOPOL_UTILITY`) slowed them down whenever anything
/// else was using the disk. (On Linux, BFQ/CFQ still derive I/O priority from the nice value.)
///
/// Call this before spawning threads: on Linux the nice value is per thread and only
/// inherited by threads created afterwards. A priority that is already lower is kept.
///
/// # Errors
///
/// * If the OS rejects the priority change
pub fn lower_process_priority() -> std::io::Result<()> {
    imp::lower()
}

#[cfg(unix)]
mod imp {
    use super::NICE;

    pub fn lower() -> std::io::Result<()> {
        // SAFETY: getpriority/setpriority take plain integers and touch no memory we own.
        let current = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
        // SAFETY: as above; `who = 0` is the calling process (thread on Linux).
        if current < NICE && unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, NICE) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::System::Threading::{
        BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, GetPriorityClass, IDLE_PRIORITY_CLASS,
        SetPriorityClass,
    };

    pub fn lower() -> std::io::Result<()> {
        // SAFETY: GetCurrentProcess returns a pseudo handle that needs no closing.
        let process = unsafe { GetCurrentProcess() };
        // SAFETY: `process` is a valid handle for the current process.
        let current = unsafe { GetPriorityClass(process) };
        if current == IDLE_PRIORITY_CLASS || current == BELOW_NORMAL_PRIORITY_CLASS {
            return Ok(());
        }
        // SAFETY: as above.
        if unsafe { SetPriorityClass(process, BELOW_NORMAL_PRIORITY_CLASS) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    #[allow(clippy::unnecessary_wraps)] // portable fallback for the platform implementations
    pub const fn lower() -> std::io::Result<()> {
        Ok(())
    }
}
