//! Background scheduling priority for the daemon process.
//!
//! The daemon is a background service: its scans should use idle CPU and disk bandwidth, not
//! compete with foreground work. Installed services get this from launchd / systemd / Task
//! Scheduler; [`lower_process_priority`] applies the same policy to a daemon spawned on demand
//! by a client.

/// Nice value applied on Unix (matches the launchd and systemd service units).
#[cfg(unix)]
const NICE: libc::c_int = 10;

/// Lowers the current process to background CPU (and, where supported, I/O) priority.
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
        if current < NICE {
            // SAFETY: as above; `who = 0` is the calling process (thread on Linux).
            if unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, NICE) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        lower_io()
    }

    /// Marks the process's disk I/O as utility class, so foreground I/O is served first.
    #[cfg(target_os = "macos")]
    fn lower_io() -> std::io::Result<()> {
        const IOPOL_TYPE_DISK: libc::c_int = 0;
        const IOPOL_SCOPE_PROCESS: libc::c_int = 0;
        const IOPOL_UTILITY: libc::c_int = 4;
        unsafe extern "C" {
            fn setiopolicy_np(
                iotype: libc::c_int,
                scope: libc::c_int,
                policy: libc::c_int,
            ) -> libc::c_int;
        }
        // SAFETY: setiopolicy_np takes plain integers and touches no memory we own.
        if unsafe { setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_PROCESS, IOPOL_UTILITY) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Linux I/O schedulers (BFQ, CFQ) derive best-effort I/O priority from the nice value.
    #[cfg(not(target_os = "macos"))]
    #[allow(clippy::unnecessary_wraps)] // signature shared with the macOS implementation
    const fn lower_io() -> std::io::Result<()> {
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
