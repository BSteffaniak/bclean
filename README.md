# bclean

Fast, action-oriented disk usage analyzer, cleaner, and monitor.

- **Browse** disk usage like ncdu, but faster (native bulk-metadata scanning on macOS, Linux,
  and Windows) and smarter (rule-aware pruning summarizes `node_modules`, `target/`, ...).
- **Clean** with knowledge of what is *ephemeral* (regenerable: build output, dependencies,
  caches) and what is *stale* (unused: idle projects, merged worktrees, old downloads).
- **Monitor** disk usage with a background daemon that raises alarms and can auto-clean,
  driven by composable TOML policies.

## Install

```sh
cargo install --path packages/bclean
```

## Usage

```sh
bclean                            # dashboard: disk, reclaimable data, where it lives (auto-starts the daemon)
bclean PATH                       # interactive browser at PATH
bclean scan ~/code -n 20          # summary + largest entries
bclean ls ~/code --stale -e       # cleanable items: ephemeral and stale, by score
bclean explain ~/code/app/target  # why something is classified the way it is
bclean clean ~/code --stale -n    # dry run; drop -n to act (asks for confirmation)
bclean undo                       # undo the last reversible action
bclean status                     # volumes, reclaimable summary, daemon, alerts
bclean history --since 7d         # what grew
bclean rules list                 # effective rules (139 built-ins + yours)
bclean daemon start|stop|status|logs
bclean service install            # keep the daemon running at login
bclean --no-daemon ...            # do everything in-process
bclean -x ...                     # stay on one filesystem (no other drives), like ncdu -x
```

## Configuration

Layers (lowest first): built-in catalog → `~/.config/bclean/config.toml` (or `$BCLEAN_CONFIG`)
→ nearest `.bclean.toml` → `--config FILE`. Rules, policies, and alerts are keyed tables, so a
layer can override a single field of a built-in rule.

```toml
[rules.cargo_target]
auto = false                      # tweak a built-in

[rules.my_renders]
match = { name = ["renders"], sibling_any = ["project.blend"] }
ephemeral = { kind = "BUILD_ARTIFACT", regen_cost = "EXPENSIVE" }
action = { kind = "TRASH" }

[policies.low_disk]
trigger = { kind = "FREE_BELOW", volume = "/", threshold = "10%" }
target = { free_above = "20%" }
mode = "SHADOW"                   # NOTIFY_ONLY | SHADOW | ENFORCE
notify = ["desktop"]

[alerts.desktop]
kind = "DESKTOP"
```

`bclean config show` prints the merged result. `[tui] start_view = "BROWSER"` makes plain
`bclean` open the browser instead of the dashboard.

### Drives

Besides `monitor.index_roots` (default `~`), the daemon indexes every other mounted drive
(internal disks, USB and external drives) as a root of its own, picking drives up as they are
mounted and unloading them when they go away (their index is kept for when they return).
Network, RAM-backed, and read-only filesystems, boot/EFI partitions, volumes under 1 GiB, and
the volume holding an index root (`/` when `~` is indexed) are skipped. In the browser, a
mount point (`(drive ↵)`) opens its drive; going up from the drive's top returns.

```toml
[monitor]
index_volumes = true              # false: only index_roots
exclude_volumes = ["/mnt/backup", "/run/media/*/CAMERA"]
max_watches = 0                   # Linux inotify watch cap; 0 = 40% of the per-user limit
```

`-x` / `--one-file-system` turns drive indexing off for engines the command runs
(`--no-daemon`, `daemon start/restart/run`, `service install`, `watch`) and keeps the dashboard
and browser on the browsed path's filesystem.

### Active projects

Build output of a project still being worked on is never cleaned automatically: not while the
project has been edited within the rule's `stale.project_idle` (30 days for `target/`), nor
while a process other than an idle shell (an editor, an agent, a build, a running test) works
inside it. The dashboard lists it as "active" instead of reclaimable. In a linked git worktree
whose branch was merged or deleted, build output is cleanable right away; the worktree itself is
only ever marked unused once removing it would lose nothing (a clean working tree, with its
commits on the default branch or still on the branch ref).

### Temporary files

Each entry of `/tmp`, `/var/tmp`, and macOS's per-user `$TMPDIR` (and its `X/` code-signing
clones) is cleanable once nothing inside it has been written for 3 days (7 on Windows). A
directory directly holding a socket, pipe, pid file, or lock file is left alone with everything
in it: a running program may use one for weeks without writing to it. Around it, the rest is
offered piece by piece, as are the finished runs in a folder shared by many (a tool's
`/tmp/<tool>-<uid>`) while one run still uses it. Any rule can do the same:

```toml
[rules.my_scratch]
match = { kind = "ANY", path = ["~/scratch/**"], not_live = { special = true, names = ["*.pid"] }, split = true }
ephemeral = { kind = "TEMP", regen_cost = "FREE" }
stale = { after = "3d", signals = ["FS_TIMES"] }
action = { kind = "DELETE" }
```

### Keeping outputs

Rules can name outputs worth keeping, by glob relative to the matched directory (`cargo_target`
keeps release executables and dylibs). Inside `[trim]` paths, cleaning such a match deletes
everything else instead of running the rule's action, so a main checkout keeps a runnable
`target/release/bcode` while its worktrees are cleaned completely:

```toml
[trim]
paths = ["~/GitHub/bcode", "~/GitHub/bmux"]

[rules.cargo_target]              # change what is kept
keep = [
  { glob = "release/*", executable = true },
  { glob = "release/*.{dylib,so,dll}" },
]
```

## Dashboard

Plain `bclean` opens a dashboard computed from the daemon's index (instant, no scan):

- **Disk:** usage per volume, split into reclaimable data, the rest of the index, space outside
  the index (system, apps, unindexed directories), and free space, with a 30-day trend and a
  "full in ~N days" forecast.
- **Reclaimable:** the total, how much is safe to clean right now (what an automatic clean
  would select), and the ephemeral × stale breakdown. `c` plans a clean of everything safe.
  `i` / `I` shorten or lengthen how long something must have been idle to count as safe
  (each rule's threshold, then 7d, 1d, 12h, 6h, 3h, 1h, 15m, any idle; build output goes by
  its project's idle time), for the dashboard and the clean-safe plan, which is rebuilt when
  the window changes. Items in use stay out whatever the window.
- **By kind / ecosystem** (`v`): build output, dependencies, caches, ... or rust, node, xcode,
  ... Enter opens the Reclaimable lens filtered to that row.
- **Top candidates** and **Projects**: where reclaimable data lives. Enter shows it in the
  browser; space marks a candidate (or a project's reclaimable entries, never the project).
- **Grew** (24h / 7d with `w`) and **Activity**: fastest-growing directories, space freed this
  week, alerts, and watcher health.

It updates live as the index and volume samples change.

## bmux

See [`plugins/bmux-plugin`](plugins/bmux-plugin/README.md) for the bmux integration.

## Development

Run inside `nix develop`. See [`AGENTS.md`](AGENTS.md) for required checks
(`cargo clippy --all-targets -- -D warnings`, `cargo nextest run`,
`scripts/cross-check.sh`, `cargo machete --with-metadata`) and `scripts/bench.sh` for
benchmarks.

## License

MPL-2.0
