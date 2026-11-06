//! Environmental condition probes (`ac_power_only`, `idle_for`).
//!
//! Platform behaviour (all best effort):
//!
//! * **macOS**: power source from `pmset -g batt`; idle time from `ioreg -c IOHIDSystem`
//!   (`HIDIdleTime`, nanoseconds). Both spawn a short-lived system tool.
//! * **Linux**: power source from `/sys/class/power_supply` (`Mains` online, or a
//!   `Battery` not discharging); idle time is not available (no portable API without a display
//!   server connection) and reports `None`.
//! * **Windows**: `GetSystemPowerStatus` and `GetLastInputInfo`.

use std::time::Duration;

/// Reads machine state for policy conditions.
pub trait SystemProbe {
    /// Whether the machine is on external power; `None` if unknown.
    fn on_ac_power(&self) -> Option<bool>;
    /// Time since the last user input; `None` if unknown.
    fn idle_time(&self) -> Option<Duration>;
}

/// [`SystemProbe`] backed by the operating system.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlatformProbe;

impl SystemProbe for PlatformProbe {
    fn on_ac_power(&self) -> Option<bool> {
        platform::on_ac_power()
    }

    fn idle_time(&self) -> Option<Duration> {
        platform::idle_time()
    }
}

/// Fixed answers, for tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FakeProbe {
    /// Value returned by [`SystemProbe::on_ac_power`].
    pub ac_power: Option<bool>,
    /// Value returned by [`SystemProbe::idle_time`].
    pub idle: Option<Duration>,
}

impl SystemProbe for FakeProbe {
    fn on_ac_power(&self) -> Option<bool> {
        self.ac_power
    }

    fn idle_time(&self) -> Option<Duration> {
        self.idle
    }
}

/// Parses `pmset -g batt` output.
#[must_use]
pub fn parse_pmset(output: &str) -> Option<bool> {
    if output.contains("'AC Power'") {
        Some(true)
    } else if output.contains("'Battery Power'") {
        Some(false)
    } else {
        None
    }
}

/// Parses the `HIDIdleTime` value (nanoseconds) from `ioreg -c IOHIDSystem` output.
#[must_use]
pub fn parse_hid_idle(output: &str) -> Option<Duration> {
    let line = output.lines().find(|l| l.contains("\"HIDIdleTime\""))?;
    let value = line.split('=').nth(1)?.trim();
    value.parse::<u64>().ok().map(Duration::from_nanos)
}

#[cfg(target_os = "macos")]
mod platform {
    use std::process::Command;
    use std::time::Duration;

    fn run(program: &str, args: &[&str]) -> Option<String> {
        let output = Command::new(program).args(args).output().ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }

    pub fn on_ac_power() -> Option<bool> {
        super::parse_pmset(&run("/usr/bin/pmset", &["-g", "batt"])?)
    }

    pub fn idle_time() -> Option<Duration> {
        super::parse_hid_idle(&run("/usr/sbin/ioreg", &["-c", "IOHIDSystem", "-d", "4"])?)
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::time::Duration;

    pub fn on_ac_power() -> Option<bool> {
        let entries = std::fs::read_dir("/sys/class/power_supply").ok()?;
        let read = |p: std::path::PathBuf| {
            std::fs::read_to_string(p)
                .map(|s| s.trim().to_owned())
                .unwrap_or_default()
        };
        let mut saw_battery = false;
        let mut discharging = false;
        for entry in entries.flatten() {
            let dir = entry.path();
            match read(dir.join("type")).as_str() {
                "Mains" | "USB" => {
                    if read(dir.join("online")) == "1" {
                        return Some(true);
                    }
                }
                "Battery" => {
                    saw_battery = true;
                    if read(dir.join("status")) == "Discharging" {
                        discharging = true;
                    }
                }
                _ => {}
            }
        }
        // No battery at all: a desktop, which is always on mains power.
        Some(!saw_battery || !discharging)
    }

    pub const fn idle_time() -> Option<Duration> {
        None
    }
}

#[cfg(windows)]
mod platform {
    use std::time::Duration;

    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    use windows_sys::Win32::System::SystemInformation::GetTickCount;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

    pub fn on_ac_power() -> Option<bool> {
        // SAFETY: plain C struct; all-zero is a valid initial value.
        let mut status: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
        // SAFETY: `status` is a valid, writable out pointer.
        if unsafe { GetSystemPowerStatus(&raw mut status) } == 0 {
            return None;
        }
        match status.ACLineStatus {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    pub fn idle_time() -> Option<Duration> {
        let mut info = LASTINPUTINFO {
            cbSize: u32::try_from(size_of::<LASTINPUTINFO>()).ok()?,
            dwTime: 0,
        };
        // SAFETY: `info` is valid and `cbSize` is set as the API requires.
        if unsafe { GetLastInputInfo(&raw mut info) } == 0 {
            return None;
        }
        // SAFETY: no preconditions.
        let now = unsafe { GetTickCount() };
        Some(Duration::from_millis(u64::from(
            now.wrapping_sub(info.dwTime),
        )))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
mod platform {
    use std::time::Duration;

    pub const fn on_ac_power() -> Option<bool> {
        None
    }

    pub const fn idle_time() -> Option<Duration> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pmset_and_ioreg() {
        assert_eq!(
            parse_pmset("Now drawing from 'AC Power'\n -InternalBattery-0"),
            Some(true)
        );
        assert_eq!(parse_pmset("Now drawing from 'Battery Power'"), Some(false));
        assert_eq!(parse_pmset("garbage"), None);
        let ioreg = "    | |   \"HIDIdleTime\" = 2500000000\n";
        assert_eq!(parse_hid_idle(ioreg), Some(Duration::from_millis(2500)));
        assert_eq!(parse_hid_idle("nothing"), None);
    }

    #[test]
    fn platform_probe_does_not_panic() {
        let probe = PlatformProbe;
        let _ = probe.on_ac_power();
        let _ = probe.idle_time();
    }
}
