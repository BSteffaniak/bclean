# bclean bmux plugin

A [bmux](https://github.com/BSteffaniak/bmux) native plugin (`id = "bclean"`) that surfaces
bclean disk-pressure status and alerts inside bmux:

- **`bclean status`**: daemon, volume, and alert details (plus alerts raised since the last check)
- **`bclean segment`**: a compact status segment such as `◕ 87% /  ⚠ 2`, using the most pressured
  monitored volume, colored by threshold with `--ansi`
- **`bclean ack-alerts`**: acknowledge every active alert
- **`bclean open`**: open a pane running the `bclean` TUI (through the `bmux.tabs` service)
- **Tab bar**: the same segment as a `bmux.tab_bar` status item, plus a toast for each new alert,
  published by the background monitor. bmux does not yet route these calls correctly for
  dynamically loaded plugins, so this currently disables itself; see *Limitations*.

The plugin never starts the bclean daemon unless you set `autostart = true`. While the daemon is
down the segment shows a dim `◌ bclean` (or nothing, with `offline_indicator = false`), and the
plugin keeps retrying with exponential backoff.

## Install

bmux discovers plugin manifests (`plugin.toml`) under `<bmux data dir>/plugins/` and under any
directory listed in `plugins.search_paths`. Third-party plugins are discovered but stay disabled
until you enable them.

### With the install script

```sh
plugins/bmux-plugin/scripts/install.sh --release
```

This builds `bclean_bmux_plugin` and writes `<bmux data dir>/plugins/bclean/plugin.toml`. That file
is the checked-in manifest plus an absolute `entry` path to the built library. The bmux data dir is
`$BMUX_DATA_DIR`, or `~/Library/Application Support/bmux` (macOS), `$XDG_DATA_HOME/bmux` /
`~/.local/share/bmux` (Linux), `%APPDATA%\bmux` (Windows). Use `--print-path` to see the target and
`--force` to overwrite. The script does not touch your bmux config.

### Manually

```sh
cargo build --release -p bclean_bmux_plugin
mkdir -p ~/bmux-plugins/bclean
cp target/release/libbclean_bmux_plugin.dylib ~/bmux-plugins/bclean/   # .so on Linux, .dll on Windows
cp plugins/bmux-plugin/plugin.toml ~/bmux-plugins/bclean/
# then add to ~/bmux-plugins/bclean/plugin.toml (relative to the manifest directory):
#   entry = "libbclean_bmux_plugin.dylib"
```

and add `~/bmux-plugins` to `plugins.search_paths`.

### Enable

In your bmux config (`bmux config path` prints its location):

```toml
[plugins]
enabled = ["bclean"]
# search_paths = ["~/bmux-plugins"]   # only for a manual install outside the data dir
```

Restart the bmux server (`bmux server stop`; it starts again on the next attach) so the plugin is
activated and its background monitor starts. Check it with:

```sh
bmux plugin list          # shows: bclean [enabled] - bclean (0.1.0)
bmux plugin doctor
bmux bclean status
```

The plugin must be built against the same bmux plugin ABI as your bmux binary (`native_abi` 1.x).
Rebuild it after upgrading bmux.

## Keybindings

The plugin does not claim any keys. Bind its commands yourself:

```toml
[keybindings.runtime]
"Ctrl+A B" = "plugin:bclean:open"
"Ctrl+A Shift+B" = "plugin:bclean:status"
"Ctrl+A Alt+B" = "plugin:bclean:ack-alerts"
```

From a keybinding, `status`, `ack-alerts`, and `open` report through a one-line command status
message instead of writing to the terminal.

## Shell prompt / external status bars

`bclean segment` prints the segment, so you can use it anywhere that runs a command, for example in
a zsh prompt:

```sh
RPROMPT='$(bmux bclean segment --ansi 2>/dev/null)'
```

## Settings

All settings are optional and live in the bmux config:

```toml
[plugins.settings."bclean"]
socket = "~/run/bclean.sock"   # daemon socket / pipe name; default: bclean's daemon.socket,
                               # then <bclean runtime dir>/bclean.sock (\\.\pipe\bclean-<user> on Windows)
autostart = false              # start `bclean daemon run` when the daemon is down
daemon_exe = "bclean"          # executable used for autostart
monitor = true                 # run the background monitor inside the bmux server
warning_percent = 80           # yellow at or above this usage
critical_percent = 90          # bold red at or above this usage
volumes = []                   # mount points to consider; empty = all volumes the daemon reports
show_when_ok = true            # show the volume even below the warning threshold
offline_indicator = true       # dim "◌ bclean" while the daemon is unreachable
tab_bar = true                 # mirror the segment to bmux's tab bar (status item id "disk")
notify = true                  # toast new alerts in the tab bar and queue them for the next `status`
open_program = "bclean"        # what `open` runs
open_args = []
open_layout = "SPLIT_VERTICAL" # SPLIT_VERTICAL | SPLIT_HORIZONTAL | FLOATING (lowercase also accepted)
refresh_secs = 30              # full refresh interval while connected
retry_min_secs = 2             # reconnect backoff start
retry_max_secs = 60            # reconnect backoff cap
```

Invalid settings make commands fail with a usage error. During activation they disable the monitor
with a message in the bmux server log; they never abort bmux startup.

## How it works

- **Flavor:** a `runtime = "native"` plugin (`cdylib`, `bmux_plugin_sdk::export_plugin!`). This is
  bmux's supported runtime for Rust plugins. Process plugins have no lifecycle, so they cannot hold
  a subscription open.
- **Monitor:** `activate` (in the bmux server) starts a `bclean-bmux-monitor` thread with its own
  current-thread Tokio runtime. It connects through `bclean_client` (no autostart by default), sends
  `Status`, `Volumes`, and `Alerts`, then `Subscribe { topics: [Volumes, Alerts] }`. A reader task
  forwards pushed events over a channel, a separate connection does a full refresh every
  `refresh_secs`, and on disconnect it marks the state offline and retries with backoff.
  `deactivate` stops and joins the thread.
- **Commands** read the monitor's state when they run in the same process. Otherwise (for example
  `bmux bclean ...` run from a shell, which executes in the CLI process) they make one short-lived
  query. `ack-alerts` sends `Alerts` and then one `AckAlert { key }` per alert.
- **`open`** calls the `bmux.tabs` typed service (`tabs-commands/launch-pane`, or
  `create-floating-pane`) through the generated `bmux_tabs_plugin_api` request types. The plugin
  declares `bmux.tabs.write` as an optional capability and `bmux.tabs` as an optional dependency.
  A `cdylib` links its own copy of `bmux_plugin`, whose service-location map the host never fills,
  so `open` marks `bmux.tabs` as host-routed first (as bmux does for CLI processes). This never
  overrides a provider that is local to the process, such as in a statically bundled build.
- **Tab bar:** with `monitor` and `tab_bar` on, the monitor keeps one `bmux.tab_bar` status item
  (`id = "disk"`) in sync with the segment. It sends only changes of text or severity. Severity is
  `critical`/`warning` by the volume thresholds or the worst active alert, `info` for info-only
  alerts, and `normal` otherwise. The item is cleared when the segment is empty, or while the
  daemon is offline with `offline_indicator = false` (otherwise it shows `◌ bclean`). With
  `notify`, each newly raised or escalated alert becomes a toast (title, body, and severity from
  the alert), at most once per alert key and severity. `deactivate` stops the monitor and then
  calls `clear-status-items`. The calls run on the monitor thread through the activation's
  lifecycle context. The tab-bar capabilities (`bmux.tab_bar.status.write`,
  `bmux.tab_bar.status.read`, `bmux.tab_bar.notify`) and the `bmux.tab_bar` dependency are
  optional, so the plugin still loads when the tab bar is disabled.
- **Attribution check:** the tab bar namespaces items by the caller plugin id that bmux attaches to
  each request. After its first successful `set-status-item`, the plugin calls
  `list-status-items` and continues only if the item is owned by `bclean`. If bmux attributed it to
  another id, the plugin removes the item and disables tab-bar publishing for this activation. A
  transport failure also disables it. Either way it logs one `bclean: tab-bar status and toasts
  disabled: ...` line to the bmux server's stderr. The commands keep working.

## Limitations (current bmux plugin API)

- **Tab-bar status and toasts are inactive for the dynamically loaded plugin (bmux master
  `4cd26692`).** The publishing code is in place, but bmux has no correctly attributed route from a
  `cdylib` to the bundled tab-bar plugin:
  - A `cdylib`'s typed calls go through its own copy of `bmux_plugin`. That copy has an empty
    service-location map and no bundled providers, so every call (from a command, `activate`, or
    the monitor thread) fails with `plugin 'bmux.tab_bar' is missing native entry path`. bmux's
    own `examples/native-plugin` (`status-set`, `notify`) fails the same way.
  - Forcing the host route (marking `bmux.tab_bar` remote, as `open` does for `bmux.tabs`) makes
    calls succeed, including from the monitor thread, through the process-wide host connection
    fallback. But bmux relays them over IPC `InvokeService`, which stamps
    `caller_plugin_id = "bmux.core"`, so the item would land in the `bmux.core` namespace. The
    plugin does not use that route.
  - The plugin enables itself automatically once bmux attributes the calls to `bclean`. That
    happens on a bmux with an upstream fix. It should also happen when bclean is statically bundled
    into a bmux build, where the call is dispatched in-process with the right caller (not verified
    here).
  - Until then, use `bclean segment` in a prompt or an external bar. New alerts are still queued
    for the next `bclean status`, and the bclean daemon's own alert transports (desktop
    notifications, webhooks) remain the push channel.
- Keybinding status messages from the dynamic plugin need bmux `25b200ef` or newer. That version
  reads the plugin's `bmux_plugin_take_command_outcome_v1` export, which `export_plugin!` provides.
- `open` from a shell with no attached client fails with `current client has no selected session`.
  Use it from a keybinding inside an attached session.
- `bmux plugin run bclean <cmd>` prints nothing for successful commands, because bmux does not mark
  that path as a CLI invocation. Use `bmux bclean <cmd>`.
