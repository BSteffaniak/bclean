//! Plugin settings read from `[plugins.settings."bclean"]` in the bmux configuration.

use std::time::Duration;

use serde::{Deserialize, Deserializer};

/// Default warning threshold, in percent used.
pub const DEFAULT_WARNING_PERCENT: f64 = 80.0;
/// Default critical threshold, in percent used.
pub const DEFAULT_CRITICAL_PERCENT: f64 = 90.0;

/// Where `open` places the new `bclean` pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OpenLayout {
    /// Split the focused pane side by side.
    #[default]
    #[serde(alias = "vertical", alias = "split_vertical", alias = "split-vertical")]
    SplitVertical,
    /// Split the focused pane top/bottom.
    #[serde(
        alias = "horizontal",
        alias = "split_horizontal",
        alias = "split-horizontal"
    )]
    SplitHorizontal,
    /// Open a floating pane.
    #[serde(alias = "floating")]
    Floating,
}

/// Plugin settings. Every field is optional in the bmux configuration.
// Each bool is an independent user-facing toggle mirroring a TOML key; enums would only make
// the configuration more verbose.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Daemon socket / named pipe override. Defaults to bclean's `daemon.socket`, then the
    /// platform default from `bclean_config::paths::AppDirs`.
    pub socket: Option<String>,
    /// Start the bclean daemon when it is not running. Off by default: the plugin only
    /// observes an already-running daemon.
    pub autostart: bool,
    /// Executable used to autostart the daemon (`<daemon_exe> daemon run`) and resolved from
    /// `PATH` when relative.
    pub daemon_exe: String,
    /// Run the background monitor inside the bmux server process.
    pub monitor: bool,
    /// Usage percentage at which a volume is shown as a warning.
    pub warning_percent: f64,
    /// Usage percentage at which a volume is shown as critical.
    pub critical_percent: f64,
    /// Mount points to consider (empty = every volume the daemon reports).
    pub volumes: Vec<String>,
    /// Show the segment even when no volume reaches the warning threshold.
    pub show_when_ok: bool,
    /// Show a dim indicator when the daemon is unreachable (otherwise render nothing).
    pub offline_indicator: bool,
    /// Queue new alerts for display by the next `status` invocation.
    pub notify: bool,
    /// Program launched by `open`.
    pub open_program: String,
    /// Arguments passed to `open_program`.
    pub open_args: Vec<String>,
    /// Where `open` places the pane.
    pub open_layout: OpenLayout,
    /// Seconds between full state refreshes while connected.
    #[serde(deserialize_with = "positive_secs")]
    pub refresh_secs: u64,
    /// Initial reconnect delay in seconds.
    #[serde(deserialize_with = "positive_secs")]
    pub retry_min_secs: u64,
    /// Maximum reconnect delay in seconds.
    #[serde(deserialize_with = "positive_secs")]
    pub retry_max_secs: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            socket: None,
            autostart: false,
            daemon_exe: "bclean".to_owned(),
            monitor: true,
            warning_percent: DEFAULT_WARNING_PERCENT,
            critical_percent: DEFAULT_CRITICAL_PERCENT,
            volumes: Vec::new(),
            show_when_ok: true,
            offline_indicator: true,
            notify: true,
            open_program: "bclean".to_owned(),
            open_args: Vec::new(),
            open_layout: OpenLayout::default(),
            refresh_secs: 30,
            retry_min_secs: 2,
            retry_max_secs: 60,
        }
    }
}

fn positive_secs<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let value = u64::deserialize(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom("must be at least 1 second"));
    }
    Ok(value)
}

/// Warning/critical thresholds as used fractions (`0.0..=1.0`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    /// Fraction at which a volume becomes a warning.
    pub warning: f64,
    /// Fraction at which a volume becomes critical.
    pub critical: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            warning: DEFAULT_WARNING_PERCENT / 100.0,
            critical: DEFAULT_CRITICAL_PERCENT / 100.0,
        }
    }
}

impl Settings {
    /// Deserializes settings from the host-provided value (a `toml::Value` table), or returns
    /// defaults when the plugin has no settings.
    ///
    /// # Errors
    ///
    /// * If a field has the wrong type or an invalid value
    /// * If the thresholds are outside `0..=100` or `warning_percent > critical_percent`
    pub fn from_value<'de, D: Deserializer<'de>>(value: Option<D>) -> Result<Self, String> {
        let settings = match value {
            Some(value) => Self::deserialize(value).map_err(|e| e.to_string())?,
            None => Self::default(),
        };
        settings.validate()?;
        Ok(settings)
    }

    /// Checks cross-field invariants.
    ///
    /// # Errors
    ///
    /// * If a threshold is outside `0..=100` (or not finite)
    /// * If `warning_percent` exceeds `critical_percent`
    /// * If `retry_min_secs` exceeds `retry_max_secs`
    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("warning_percent", self.warning_percent),
            ("critical_percent", self.critical_percent),
        ] {
            if !value.is_finite() || !(0.0..=100.0).contains(&value) {
                return Err(format!("{name} must be between 0 and 100 (got {value})"));
            }
        }
        if self.warning_percent > self.critical_percent {
            return Err(format!(
                "warning_percent ({}) must not exceed critical_percent ({})",
                self.warning_percent, self.critical_percent
            ));
        }
        if self.retry_min_secs > self.retry_max_secs {
            return Err(format!(
                "retry_min_secs ({}) must not exceed retry_max_secs ({})",
                self.retry_min_secs, self.retry_max_secs
            ));
        }
        Ok(())
    }

    /// The configured thresholds as fractions.
    #[must_use]
    pub fn thresholds(&self) -> Thresholds {
        Thresholds {
            warning: self.warning_percent / 100.0,
            critical: self.critical_percent / 100.0,
        }
    }

    /// Interval between full refreshes.
    #[must_use]
    pub const fn refresh_interval(&self) -> Duration {
        Duration::from_secs(self.refresh_secs)
    }

    /// The configured socket override, ignoring blank values.
    #[must_use]
    pub fn socket_override(&self) -> Option<&str> {
        self.socket
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Settings, String> {
        let value: toml::Value = toml::from_str(text).unwrap();
        Settings::from_value(Some(value))
    }

    #[test]
    fn missing_settings_use_defaults() {
        let settings = Settings::from_value::<toml::Value>(None).unwrap();
        assert_eq!(settings, Settings::default());
        assert!(!settings.autostart);
        assert!(settings.monitor);
    }

    #[test]
    fn parses_overrides() {
        let settings = parse(
            r#"
            socket = "/tmp/b.sock"
            autostart = true
            warning_percent = 70
            critical_percent = 95.5
            volumes = ["/", "/data"]
            open_layout = "floating"
            open_args = ["--no-daemon"]
            "#,
        )
        .unwrap();
        assert_eq!(settings.socket_override(), Some("/tmp/b.sock"));
        assert!(settings.autostart);
        assert!((settings.thresholds().warning - 0.70).abs() < 1e-9);
        assert!((settings.thresholds().critical - 0.955).abs() < 1e-9);
        assert_eq!(settings.volumes, ["/", "/data"]);
        assert_eq!(settings.open_layout, OpenLayout::Floating);
        assert_eq!(
            parse("open_layout = \"SPLIT_HORIZONTAL\"")
                .unwrap()
                .open_layout,
            OpenLayout::SplitHorizontal
        );
    }

    #[test]
    fn rejects_invalid_values() {
        assert!(parse("warning_percent = 95\ncritical_percent = 90").is_err());
        assert!(parse("critical_percent = 120").is_err());
        assert!(parse("refresh_secs = 0").is_err());
        assert!(parse("retry_min_secs = 10\nretry_max_secs = 5").is_err());
        assert!(parse("autostart = \"yes\"").is_err());
    }

    #[test]
    fn blank_socket_is_ignored() {
        assert_eq!(parse("socket = \"  \"").unwrap().socket_override(), None);
    }
}
