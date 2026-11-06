# bclean Agent Guidelines

bclean is a fast, action-oriented disk usage analyzer (ncdu-style), cleaner, and monitor.
See `docs/PLAN.md` for the architecture.

## Build/Test Commands

- **Rust build**: `cargo build`
- **Rust test**: `cargo nextest run --no-fail-fast` (or `cargo test`), `cargo test -p <package>` (single package)
- **Rust lint**: `cargo clippy --all-targets -- -D warnings` (zero warnings is REQUIRED)
- **Cross-platform lint**: `nix develop --command scripts/cross-check.sh` (macOS, Linux, Windows)
- **Unused deps**: `cargo machete --with-metadata` (zero findings is REQUIRED)
- **Format**: `cargo fmt` (Rust) for ALL packages in the workspace; `taplo fmt` for TOML
- **Install globally**: `cargo install --path packages/bclean`
- **Benchmarks**: `scripts/bench.sh` (compares against ncdu/gdu/dua via hyperfine)

## Code Style Guidelines

### Rust Patterns

- **Collections**: Always use `BTreeMap`/`BTreeSet`, never `HashMap`/`HashSet`
- **Dependencies**: Use `workspace = true`, never path dependencies or inline versions
- **New Dependencies**: When adding a new dependency:
    - Add to workspace `Cargo.toml` with `default-features = false`
    - Specify full version including patch (e.g., `"0.4.28"` not `"0.4"`)
    - Verify you're using the LATEST stable version from crates.io
    - In package `Cargo.toml`, use `workspace = true` and opt-in to specific features only
- **Clippy**: Required in every crate root:
    ```rust
    #![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
    #![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
    #![allow(clippy::multiple_crate_versions)]
    ```
- Fix clippy root causes. No crate-level `allow`s beyond the header; item-level `#[allow]` only
  with a comment explaining why.
- No wildcard imports outside `#[cfg(test)] mod tests { use super::*; }`.
- **Unsafe**: Only for platform FFI (syscalls, OS APIs) in platform-specific modules; never in
  `bclean_model`, `bclean_tree`, or pure logic. Every `unsafe` block needs a `// SAFETY:` comment.
- **Databases**: All database access goes through `switchy_database` /
  `switchy_database_connection` (SQLite via `sqlite-rusqlite`), never raw `rusqlite`.
- **Rustdoc Error docs**: Use asterisks (*) for bullet points, document all error conditions
- **Must use**: Add `#[must_use]` to constructors and getters that return types OTHER THAN Result or Option

### Package Organization

- **Naming**: All packages use underscore naming (`bclean_scan`); the binary package is `bclean`
- **Features**: Always include `fail-on-warnings = []` feature and propagate it to internal deps
- **Serde**: Use `SCREAMING_SNAKE_CASE` for enum value renames; struct keys are `snake_case`

### Architecture Boundaries

- `bclean_model` and `bclean_tree` are the shared contracts. Keep them dependency-light and
  free of I/O.
- `bclean_scan` must not depend on `bclean_rules`; pruning is injected via its `PruneOracle` trait.
- Platform-specific code lives behind `cfg(target_os = ...)` modules with a portable fallback.
  macOS, Linux, and Windows are all first-class: code must compile on all three.
- Destructive operations live only in `bclean_actions`, which enforces protected paths,
  journals every action, and supports dry runs.
- The TUI builds on `bmux_tui`, `bmux_tui_components`, and `bmux_tui_runtime`. Prefer upstreaming
  missing generic components to bmux over local reimplementation.

### Documentation

- Document all public APIs with comprehensive error information
- Include examples for complex functions
