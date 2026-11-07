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

## Dashboard

Plain `bclean` opens a dashboard computed from the daemon's index (instant, no scan):

- **Disk:** usage per volume, split into reclaimable data, the rest of the index, space outside
  the index (system, apps, unindexed directories), and free space, with a 30-day trend and a
  "full in ~N days" forecast.
- **Reclaimable:** the total, how much is safe to clean right now (what an automatic clean
  would select), and the ephemeral × stale breakdown. `c` plans a clean of everything safe.
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
