# bclean

Fast, action-oriented disk usage analyzer, cleaner, and monitor.

- **Browse** disk usage like ncdu, but faster (native bulk-metadata scanning on macOS, Linux,
  and Windows) and smarter (rule-aware pruning).
- **Clean** with knowledge of what is *ephemeral* (regenerable: build output, dependencies,
  caches) and what is *stale* (unused: idle projects, merged worktrees, old downloads).
- **Monitor** disk usage with a background daemon that raises alarms and can auto-clean,
  driven by composable TOML policies.

See [`docs/PLAN.md`](docs/PLAN.md) for the architecture.

## Install

```sh
cargo install --path packages/bclean
```

## License

MPL-2.0
