# bclean bmux plugin

A [bmux](https://github.com/BSteffaniak/bmux) native plugin (`id = "bclean"`) that surfaces
bclean disk-pressure status and alerts inside bmux:

- **`bclean status`**: daemon, volume, and alert details (plus alerts raised since the last check)
- **`bclean segment`**: a compact status segment such as `◕ 87% /  ⚠ 2`, using the most pressured
  monitored volume, colored by threshold with `--ansi`
- **`bclean ack-alerts`**: acknowledge every active alert
- **`bclean open`**: open a pane running the `bclean` TUI (through the `bmux.tabs` service)

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
message instead of writing to the terminal (see *Limitations*).

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
notify = true                  # queue new alerts for the next `status`
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

## Limitations (current bmux plugin API)

- **No status-bar segment inside bmux.** The tab/status bar (`bmux.tab_bar`) has no extension point
  for third-party modules. Retained layout/surface publication (`bmux_plugin::layout` /
  `surface`) works only through in-process attach companions, and only bundled plugins compiled
  into bmux can register those. A `cdylib` has its own copy of those global registries. Use
  `bclean segment` in a prompt or an external bar instead.
- **No toasts.** bmux has no notification API for plugins, and attach prompts are in-process only.
  New alerts are queued and shown by the next `bclean status`. The bclean daemon's own alert
  transports (desktop notifications, webhooks) remain the push channel.
- **Keybinding feedback from a dynamic plugin is lost.** `status` records the
  `bmux.status_message` command outcome. bmux reads that through the SDK's thread-local capture,
  which a separately compiled `cdylib` does not share, so the message only shows when the plugin is
  statically bundled. CLI invocations print normally.
- `bmux plugin run bclean <cmd>` prints nothing for successful commands, because bmux does not mark
  that path as a CLI invocation. Use `bmux bclean <cmd>`.
