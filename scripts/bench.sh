#!/usr/bin/env bash
# Benchmarks the bclean scanner against ncdu, gdu, dua and du with hyperfine (warm cache).
#
# Usage:
#   scripts/bench.sh [PATH...]
#
# With no PATH, a deterministic fixture (~200k files) is generated in a temp directory first.
# Tools come from the Nix dev shell; the script re-executes itself inside `nix develop` when
# hyperfine is not on PATH. All scanned paths are only read.
#
# Environment:
#   RUNS      hyperfine runs per command (default 5)
#   WARMUP    hyperfine warmup runs per command (default 1)
#   THREADS   threads for multi-threaded tools (default: CPU count)
#   OUT       directory for hyperfine JSON/Markdown exports (default: target/bench)
#   SKIP      space-separated tools to skip (e.g. "ncdu1 du")
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if ! command -v hyperfine >/dev/null 2>&1; then
    exec nix develop "$ROOT" --command bash "$ROOT/scripts/bench.sh" "$@"
fi

RUNS="${RUNS:-5}"
WARMUP="${WARMUP:-1}"
THREADS="${THREADS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc)}"
OUT="${OUT:-$ROOT/target/bench}"
SKIP=" ${SKIP:-} "
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"

cd "$ROOT"
cargo build --release -p bclean_scan --example scan --example gen_fixture >&2
BIN="$CARGO_TARGET_DIR/release/examples/scan"
GEN="$CARGO_TARGET_DIR/release/examples/gen_fixture"

paths=("$@")
if [ ${#paths[@]} -eq 0 ]; then
    fixture="$(mktemp -d "${TMPDIR:-/tmp}/bclean-bench.XXXXXX")/fixture"
    trap 'rm -rf "$(dirname "$fixture")"' EXIT
    "$GEN" "$fixture" >&2
    paths=("$fixture")
fi

mkdir -p "$OUT"
skip() { [[ "$SKIP" == *" $1 "* ]]; }

for path in "${paths[@]}"; do
    echo "== $path" >&2
    "$BIN" "$path" >&2
    cmds=()
    names=()
    add() {
        if ! skip "$1"; then
            names+=("$1")
            cmds+=("$2")
        fi
    }
    add bclean "$BIN '$path'"
    add bclean-portable "$BIN --portable '$path'"
    command -v ncdu >/dev/null && add ncdu1 "ncdu -0 -o /dev/null '$path'"
    command -v ncdu >/dev/null && add ncdu "ncdu -0 -t $THREADS -o /dev/null '$path'"
    # gdu skips the scan entirely when stdout is /dev/null, so pipe through cat.
    command -v gdu >/dev/null && add gdu "gdu -n -p '$path' | cat >/dev/null"
    command -v dua >/dev/null && add dua "dua '$path' | cat >/dev/null"
    add du "du -sk '$path' >/dev/null"

    args=()
    for i in "${!cmds[@]}"; do
        args+=(--command-name "${names[$i]}" "${cmds[$i]}")
    done
    slug="$(echo "$path" | tr '/ ' '__')"
    hyperfine --shell=default --warmup "$WARMUP" --runs "$RUNS" --ignore-failure \
        --export-json "$OUT/bench$slug.json" \
        --export-markdown "$OUT/bench$slug.md" \
        "${args[@]}"
done
