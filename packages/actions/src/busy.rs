//! Best-effort detection of targets that are currently in use.
//!
//! Two independent checks are performed:
//!
//! * **Lock files**: well-known build-tool and package-manager lock/state files inside the
//!   target (for example `target/debug/.cargo-lock`, `target/.rustc_info.json`,
//!   `node_modules/.package-lock.json`) or next to it (`Cargo.lock`, `package-lock.json`,
//!   `yarn.lock`, ...) that were modified within a recent window indicate a build or install in
//!   progress.
//! * **Process working directories**: a process whose current directory is inside the target.
//!   Linux reads `/proc/*/cwd`; macOS uses libproc `proc_pidinfo(PROC_PIDVNODEPATHINFO)` (only
//!   processes owned by the current user are visible without privileges). Windows has no cheap
//!   API for this and the check is skipped.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Lock/state files looked for *inside* the target.
const INNER_LOCKS: &[&str] = &[
    ".cargo-lock",
    "debug/.cargo-lock",
    "release/.cargo-lock",
    ".rustc_info.json",
    ".package-lock.json",
    ".yarn-integrity",
    ".modules.yaml",
    ".pnpm/lock.yaml",
    ".yarn-state.yml",
    "pip-delete-this-directory.txt",
    ".gradle/noVersion/buildLogic.lock",
    "build/.lock",
];

/// Package-manager lock files looked for *next to* the target.
const SIBLING_LOCKS: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lockb",
    "bun.lock",
    "poetry.lock",
    "uv.lock",
    "Pipfile.lock",
    "Gemfile.lock",
    "composer.lock",
    "go.sum",
];

/// Checks whether `path` appears to be in use.
///
/// Returns a human-readable reason, or `None` when nothing indicates activity. `window` is how
/// recently a lock file must have been modified to count; a zero window disables the lock-file
/// check.
#[must_use]
pub fn check_busy(path: &Path, window: Duration, check_processes: bool) -> Option<String> {
    check_busy_except(path, window, check_processes, &|_| false)
}

/// Like [`check_busy`], ignoring processes working in a directory `spared` returns `true` for
/// (relative to `path`): what a trim keeps, such as a database kept in `target/`.
#[must_use]
pub fn check_busy_except(
    path: &Path,
    window: Duration,
    check_processes: bool,
    spared: &dyn Fn(&Path) -> bool,
) -> Option<String> {
    if !window.is_zero() {
        let now = SystemTime::now();
        let recent = |candidate: &Path| -> bool {
            std::fs::symlink_metadata(candidate)
                .and_then(|m| m.modified())
                .is_ok_and(|mtime| now.duration_since(mtime).map_or(true, |age| age < window))
        };
        let is_dir = std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
        if is_dir {
            for rel in INNER_LOCKS {
                let candidate = path.join(rel);
                if recent(&candidate) {
                    return Some(format!("{} was modified recently", candidate.display()));
                }
            }
        }
        if let Some(parent) = path.parent() {
            for name in SIBLING_LOCKS {
                let candidate = parent.join(name);
                if recent(&candidate) {
                    return Some(format!("{} was modified recently", candidate.display()));
                }
            }
        }
    }
    if check_processes {
        let target = std::fs::canonicalize(path).ok()?;
        if let Some(p) = working_processes()
            .into_iter()
            .find(|p| p.cwd.strip_prefix(&target).is_ok_and(|rel| !spared(rel)))
        {
            return Some(format!(
                "process {} ({}) has its working directory inside",
                p.pid, p.name
            ));
        }
    }
    None
}

/// Returns the pid of a process whose working directory is inside `path`, if detectable.
#[must_use]
pub fn process_with_cwd_inside(path: &Path) -> Option<u32> {
    let target = std::fs::canonicalize(path).ok()?;
    working_processes()
        .into_iter()
        .find(|p| p.cwd.starts_with(&target))
        .map(|p| p.pid)
}

/// A process and the directory it works in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkingProcess {
    /// Process id.
    pub pid: u32,
    /// Executable name (`bmux`, `cargo`, `zsh`, ...).
    pub name: String,
    /// Current working directory.
    pub cwd: PathBuf,
    /// CPU time used so far (user and system), where the platform reports it.
    pub cpu_ns: Option<u64>,
}

impl WorkingProcess {
    /// Whether this is an interactive shell. A shell left open in a project says little about
    /// whether the project is being worked on; what runs inside it does.
    #[must_use]
    pub fn is_shell(&self) -> bool {
        SHELLS.contains(&self.name.trim_start_matches('-'))
    }
}

/// Share of a core a process must have used since the previous sample to count as active.
///
/// Programs left open idle (an editor, a git UI polling `git status`, an agent session
/// between turns) were measured at 0.1-2%; a build, a test run, or an agent at work runs at
/// tens of percent or more.
pub const ACTIVE_CPU_PERCENT: u64 = 5;

/// Gap between the two readings of [`ActivitySampler`]'s first sample.
const FIRST_SAMPLE_GAP: Duration = Duration::from_millis(250);

/// Tells processes doing something from processes merely left open.
///
/// A git UI, an idle agent session, or a dev server nobody calls is open but idle: a process is
/// active when it used at least [`ACTIVE_CPU_PERCENT`] of a core since the previous sample.
#[derive(Debug, Default)]
pub struct ActivitySampler {
    last: Option<(std::time::Instant, BTreeMap<u32, u64>)>,
}

impl ActivitySampler {
    /// Working processes, each with whether it is active. The first call reads twice,
    /// [`FIRST_SAMPLE_GAP`] apart; later calls compare with the previous one. A process
    /// whose CPU time is unknown counts as active.
    pub fn sample(&mut self) -> Vec<(WorkingProcess, bool)> {
        let (then, before) = self.last.take().unwrap_or_else(|| {
            let first = Self::cpu_times(&working_processes());
            std::thread::sleep(FIRST_SAMPLE_GAP);
            first
        });
        let processes = working_processes();
        let (now, cpu) = Self::cpu_times(&processes);
        self.last = Some((now, cpu));
        let budget = u64::try_from(now.duration_since(then).as_nanos()).unwrap_or(u64::MAX) / 100
            * ACTIVE_CPU_PERCENT;
        processes
            .into_iter()
            .map(|p| {
                let active = match (p.cpu_ns, before.get(&p.pid)) {
                    (Some(now), Some(before)) => now.saturating_sub(*before) >= budget,
                    // New since the previous sample: started recently, so doing something.
                    (Some(_), None) | (None, _) => true,
                };
                (p, active)
            })
            .collect()
    }

    fn cpu_times(processes: &[WorkingProcess]) -> (std::time::Instant, BTreeMap<u32, u64>) {
        (
            std::time::Instant::now(),
            processes
                .iter()
                .filter_map(|p| p.cpu_ns.map(|cpu| (p.pid, cpu)))
                .collect(),
        )
    }
}

/// Interactive shells (see [`WorkingProcess::is_shell`]).
const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "nu", "dash", "ksh", "tcsh", "csh", "elvish", "xonsh", "pwsh",
    "login",
];

/// Processes of the current user (other than this one) with their working directories, where
/// the platform exposes them (Linux and macOS; empty elsewhere).
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[must_use]
pub fn working_processes() -> Vec<WorkingProcess> {
    platform::working_processes()
}

/// Processes of the current user with their working directories: none, as this platform has
/// no cheap API for them.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[must_use]
pub const fn working_processes() -> Vec<WorkingProcess> {
    Vec::new()
}

#[cfg(target_os = "linux")]
mod platform {
    use std::path::Path;

    use super::WorkingProcess;

    /// User plus system CPU time from `/proc/<pid>/stat` (fields 14 and 15, in clock ticks).
    fn cpu_ns(proc_dir: &Path) -> Option<u64> {
        let stat = std::fs::read_to_string(proc_dir.join("stat")).ok()?;
        // The command name (field 2) may contain spaces; fields after it are plain.
        let rest = &stat[stat.rfind(')')? + 1..];
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let ticks = fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?;
        // SAFETY: sysconf has no preconditions.
        let per_sec = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).ok()?;
        (per_sec > 0).then(|| ticks * 1_000_000_000 / per_sec)
    }

    pub fn working_processes() -> Vec<WorkingProcess> {
        let own = std::process::id();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
                if pid == own {
                    return None;
                }
                let cwd = std::fs::read_link(entry.path().join("cwd")).ok()?;
                let name = std::fs::read_to_string(entry.path().join("comm"))
                    .map(|s| s.trim().to_owned())
                    .unwrap_or_default();
                let cpu_ns = cpu_ns(&entry.path());
                Some(WorkingProcess {
                    pid,
                    name,
                    cwd,
                    cpu_ns,
                })
            })
            .collect()
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::CStr;
    use std::path::PathBuf;

    use super::WorkingProcess;

    pub fn working_processes() -> Vec<WorkingProcess> {
        let own = std::process::id();
        all_pids()
            .into_iter()
            .filter_map(|pid| {
                let upid = u32::try_from(pid).ok()?;
                if upid == 0 || upid == own {
                    return None;
                }
                let cwd = cwd_of(pid)?;
                Some(WorkingProcess {
                    pid: upid,
                    name: name_of(pid),
                    cwd,
                    cpu_ns: cpu_ns(pid),
                })
            })
            .collect()
    }

    fn all_pids() -> Vec<libc::c_int> {
        let mut pids: Vec<libc::c_int> = vec![0; 8192];
        loop {
            let Ok(bytes) = libc::c_int::try_from(pids.len() * size_of::<libc::c_int>()) else {
                return Vec::new();
            };
            // SAFETY: the buffer is valid for `bytes` bytes of writable `c_int`s.
            let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
            let Ok(count) = usize::try_from(count) else {
                return Vec::new();
            };
            if count < pids.len() || pids.len() >= 1 << 20 {
                pids.truncate(count);
                return pids;
            }
            pids.resize(pids.len() * 2, 0);
        }
    }

    fn cwd_of(pid: libc::c_int) -> Option<PathBuf> {
        // SAFETY: `proc_vnodepathinfo` is a plain C struct for which all-zero bytes is valid.
        let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = libc::c_int::try_from(size_of::<libc::proc_vnodepathinfo>()).ok()?;
        // SAFETY: `info` is a valid, writable buffer of `size` bytes for this flavor.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                std::ptr::addr_of_mut!(info).cast(),
                size,
            )
        };
        if written != size {
            return None;
        }
        let raw = &info.pvi_cdir.vip_path;
        // SAFETY: `vip_path` is a contiguous `[[c_char; 32]; 32]` (MAXPATHLEN bytes) array;
        // viewing it as a flat byte slice of the same length is sound.
        let bytes: &[u8] =
            unsafe { std::slice::from_raw_parts(raw.as_ptr().cast::<u8>(), size_of_val(raw)) };
        let text = CStr::from_bytes_until_nul(bytes).ok()?.to_str().ok()?;
        (!text.is_empty()).then(|| PathBuf::from(text))
    }

    /// User plus system CPU time from `PROC_PIDTASKINFO` (in Mach time units, converted).
    fn cpu_ns(pid: libc::c_int) -> Option<u64> {
        // SAFETY: `proc_taskinfo` is a plain C struct for which all-zero bytes is valid.
        let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let size = libc::c_int::try_from(size_of::<libc::proc_taskinfo>()).ok()?;
        // SAFETY: `info` is a valid, writable buffer of `size` bytes for this flavor.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTASKINFO,
                0,
                std::ptr::addr_of_mut!(info).cast(),
                size,
            )
        };
        if written != size {
            return None;
        }
        let (numer, denom) = timebase();
        let ticks = u128::from(info.pti_total_user) + u128::from(info.pti_total_system);
        u64::try_from(ticks * u128::from(numer) / u128::from(denom.max(1))).ok()
    }

    /// Mach time units per nanosecond, as `numer / denom`.
    #[allow(deprecated)] // libc points to the mach2 crate; one call doesn't justify it
    fn timebase() -> (u32, u32) {
        static TIMEBASE: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
        *TIMEBASE.get_or_init(|| {
            let mut info = libc::mach_timebase_info { numer: 1, denom: 1 };
            // SAFETY: `info` is a valid, writable mach_timebase_info.
            if unsafe { libc::mach_timebase_info(&raw mut info) } != 0 {
                return (1, 1);
            }
            (info.numer, info.denom)
        })
    }

    fn name_of(pid: libc::c_int) -> String {
        let mut buf = [0u8; 256];
        let Ok(len) = u32::try_from(buf.len()) else {
            return String::new();
        };
        // SAFETY: `buf` is valid for `len` writable bytes; proc_name NUL-terminates within it.
        let written = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), len) };
        if written <= 0 {
            return String::new();
        }
        CStr::from_bytes_until_nul(&buf)
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_cargo_lock_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir_all(target.join("debug")).unwrap();
        assert!(check_busy(&target, Duration::from_secs(60), false).is_none());
        std::fs::write(target.join("debug/.cargo-lock"), b"").unwrap();
        assert!(check_busy(&target, Duration::from_secs(60), false).is_some());
        assert!(check_busy(&target, Duration::ZERO, false).is_none());
    }

    #[test]
    fn recent_sibling_lockfile_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        let modules = dir.path().join("node_modules");
        std::fs::create_dir(&modules).unwrap();
        std::fs::write(dir.path().join("package-lock.json"), b"{}").unwrap();
        assert!(check_busy(&modules, Duration::from_secs(60), false).is_some());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn detects_child_process_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .current_dir(dir.path())
            .spawn()
            .unwrap();
        // Give the child a moment to start.
        let mut found = None;
        for _ in 0..50 {
            found = process_with_cwd_inside(dir.path());
            if found.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        child.kill().ok();
        child.wait().ok();
        assert_eq!(found, Some(child.id()));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn lists_working_processes_with_their_names() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let mut found = None;
        for _ in 0..50 {
            found = working_processes()
                .into_iter()
                .find(|p| p.pid == child.id());
            if found.as_ref().is_some_and(|p| !p.name.is_empty()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        child.kill().ok();
        child.wait().ok();
        let found = found.expect("the child is listed");
        // `sleep` may be a multi-call binary (`coreutils` under Nix): only its presence counts.
        assert!(!found.name.is_empty() && !found.is_shell(), "{found:?}");
        let shell = WorkingProcess {
            name: "-zsh".to_owned(),
            ..found
        };
        assert!(shell.is_shell());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_busy_process_is_active_and_a_sleeping_one_is_not() {
        let mut sleeper = std::process::Command::new("sleep")
            .arg("10")
            .spawn()
            .unwrap();
        let mut spinner = std::process::Command::new("sh")
            .args(["-c", "while :; do :; done"])
            .spawn()
            .unwrap();
        let mut sampler = ActivitySampler::default();
        let _ = sampler.sample();
        std::thread::sleep(Duration::from_millis(300));
        let sample = sampler.sample();
        sleeper.kill().ok();
        spinner.kill().ok();
        sleeper.wait().ok();
        spinner.wait().ok();
        let active = |pid: u32| sample.iter().find(|(p, _)| p.pid == pid).map(|(_, a)| *a);
        assert_eq!(active(sleeper.id()), Some(false));
        assert_eq!(active(spinner.id()), Some(true));
    }
}
