#!/usr/bin/env bash
# Type-check (clippy) the workspace for every supported target from one host.
#
# `cargo check` never links, so C dependencies that can't be cross-compiled here (bundled
# SQLite pulled in by switchy_database) are satisfied by no-op cc/ar stubs. Run inside
# `nix develop`, which provides the cross std targets.
#
# Usage: scripts/cross-check.sh [cargo clippy args...]   (default: --workspace --all-targets)
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
stub="$root/scripts/check-cc-stub"
targets=(aarch64-apple-darwin x86_64-unknown-linux-gnu x86_64-pc-windows-msvc)
args=("$@")
[ ${#args[@]} -eq 0 ] && args=(--workspace --all-targets)

host="$(rustc -vV | sed -n 's/^host: //p')"
status=0
for target in "${targets[@]}"; do
    echo "==> $target"
    env_vars=()
    if [ "$target" != "$host" ]; then
        var="${target//-/_}"
        env_vars=("CC_$var=$stub/cc" "AR_$var=$stub/ar")
    fi
    if ! env "${env_vars[@]}" cargo clippy --target "$target" "${args[@]}" -- -D warnings; then
        status=1
    fi
done
exit $status
