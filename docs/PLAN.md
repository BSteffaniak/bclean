# bclean architecture plan

bclean is an ncdu-style disk usage browser that is also a cleaner (it knows what is
**ephemeral** and what is **stale**) and a monitor (a daemon watches disk usage, raises
alarms, and can auto-clean).

## Core concepts

|               | Fresh                     | Stale                          |
| ------------- | ------------------------- | ------------------------------ |
| **Ephemeral** | clean under pressure      | prime auto-clean candidates    |
| **Durable**   | leave alone               | review suggestions (trash)     |

- **Ephemeral** = regenerable (what it *is*): build output, dependencies, caches, logs, temp,
  simulators, container layers. Each rule declares `kind` and `regen_cost`.
- **Stale** = unused (how it is *used*), from four signal providers:
  - `FS_TIMES`: newest mtime/atime/ctime in the subtree
  - `PROJECT_ACTIVITY`: owning project's last commit / HEAD reflog / source mtimes
  - `OS_LAST_USED`: Spotlight `kMDItemLastUsedDate`, Windows last access
  - `GIT_STATE`: merged/deleted-branch worktrees, orphaned worktree dirs
- **Confidence**: anchored detection (`target/` only with sibling `Cargo.toml`), `CACHEDIR.TAG`
  is always high confidence, a gitignored subtree is a low-confidence guess.
- **Score** = reclaimable bytes × staleness factor × confidence ÷ regen cost.
- `bclean explain <path>` shows the rule, signals, score, and action.

## Crates

| crate               | role                                                                   |
| ------------------- | ---------------------------------------------------------------------- |
| `bclean`            | binary                                                                 |
| `bclean_cli`        | clap commands; auto-spawns the daemon                                  |
| `bclean_model`      | shared domain + config schema types (contract)                         |
| `bclean_tree`       | compact SoA arena tree with aggregates (contract)                      |
| `bclean_config`     | layered loading via `bmux_config_composition` (catalog → global → project) |
| `bclean_catalog`    | built-in rules as embedded TOML                                        |
| `bclean_rules`      | matcher, classifier, scorer                                            |
| `bclean_staleness`  | signal providers (fs, project, OS metadata, git via gix)               |
| `bclean_scan`       | parallel walker + native bulk-metadata backends                        |
| `bclean_index`      | persistent snapshots, history rollups, action journal (SQLite)         |
| `bclean_watch`      | FS event sources + volume usage polling                                |
| `bclean_actions`    | delete / trash / command / archive / move; protected paths; journal; undo |
| `bclean_policy`     | triggers, auto-clean planning, shadow mode                             |
| `bclean_alerts`     | desktop, command hook, webhook, bmux                                   |
| `bclean_protocol`   | versioned IPC messages                                                 |
| `bclean_client`     | IPC client + daemon autostart                                          |
| `bclean_daemon`     | server, index ownership, watchers, policies, service install           |
| `bclean_tui`        | interactive UI on bmux TUI                                             |

## Scanner

1. Bulk metadata: macOS `getattrlistbulk`; Linux `getdents64` + `statx` (minimal mask);
   Windows `NtQueryDirectoryFile` (sizes come with enumeration), optional MFT reader when
   elevated. Portable `std::fs` fallback.
2. Work-stealing parallel traversal per directory; (dev, ino) hard-link dedupe only for
   `nlink > 1`; stays on one filesystem; allocated size by default.
3. Rule-aware pruning: directories matching ephemeral rules are summarized (sizes only, no child
   nodes) via the `PruneOracle` trait; expanded lazily on demand.
4. Streaming progress so the UI is usable mid-scan.
5. Benchmarks vs ncdu/gdu/dua (`scripts/bench.sh`).

## Index & incremental updates

- Snapshot of `TreeParts` on disk; SQLite for history rollups and the action journal.
- macOS FSEvents and Windows USN journal replay changes since the last event id, even after
  downtime. Linux uses fanotify when privileged, else inotify + periodic dirty rescans.
- Change events name directories whose *own entries* changed. Each is reconciled shallowly:
  re-list that one directory, update changed entries, drop removed ones, and scan only new
  subdirectories. Unchanged subtrees are never read, so a dotfile saved in `~` costs one
  directory listing, not a rescan of the home directory. Subtree rescans happen only when the
  event history is incomplete (dropped events, FSEvents `MustScanSubDirs`, root moved).
- Changes inside a summarized directory (`target/` during a build) re-measure it at most once
  per `monitor.summary_refresh_interval` (default 1m).
- bclean's own data, cache, and runtime directories are ignored by the watcher, so its snapshot
  and database writes never feed back into the index.
- Incremental updates are saved when an IPC client needs the snapshot file (a request, or a
  `TREE_UPDATED` event to a subscriber, at most once a second) and otherwise after
  `monitor.persist_interval` (default 5m) and at shutdown. The watch cursor is saved with the
  snapshot, so unsaved changes are replayed after a crash.
- After any action, measure actual freed space via volume stats (APFS clones/snapshots make
  estimates fuzzy).

## Daemon

- `bclean` connects to the daemon and spawns it detached if absent; version handshake restarts
  stale daemons. `bclean watch` runs the same engine in the foreground; `--no-daemon` does a
  standalone scan.
- Unix socket (macOS/Linux) / named pipe (Windows), newline-delimited JSON frames.
- `bclean service install` registers launchd / systemd `--user` / Task Scheduler.

## Configuration

Rules, policies, and alerts are keyed maps so layers compose (a user layer can set
`[rules.cargo_target] auto = false` to tweak a built-in). Layers: built-in catalog →
`~/.config/bclean/config.toml` → `<project>/.bclean.toml`. Lua scripting is a future layer and
rules-engine implementation.

## Safety

Never-touch list; skip busy targets (lock files, processes with cwd inside); per-run budgets;
battery/idle conditions; every action journaled; `bclean undo` for reversible actions; new
policies default to `SHADOW` mode.

## Phases

0. Scaffold. 1. Scanner + tree + benches. 2. Config/catalog/rules/staleness/actions + `ls`,
`explain`, `clean`. 3. TUI. 4. Index + daemon + IPC. 5. Watchers + alerts. 6. Policies.
7. bmux plugin, service install, MFT, history UI. 8. Lua.
