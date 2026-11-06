#!/usr/bin/env bash
# Build the bclean bmux plugin and install a manifest into the bmux plugins directory.
#
# Usage: plugins/bmux-plugin/scripts/install.sh [--release] [--force] [--print-path]
#
# The installed manifest is the checked-in plugin.toml plus an absolute `entry` pointing at the
# built library. It does not edit your bmux config: enable the plugin yourself (see README.md).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
plugin_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
profile=debug
force=0
print_path=0

for arg in "$@"; do
    case "$arg" in
        --release) profile=release ;;
        --force) force=1 ;;
        --print-path) print_path=1 ;;
        -h | --help)
            sed -n '2,7p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "unknown option: $arg" >&2
            exit 1
            ;;
    esac
done

# Mirrors bmux's data directory resolution (`BMUX_DATA_DIR`, else the platform data dir + bmux).
data_dir() {
    if [[ -n "${BMUX_DATA_DIR:-}" ]]; then
        printf '%s\n' "$BMUX_DATA_DIR"
        return
    fi
    case "$(uname -s)" in
        Darwin) printf '%s\n' "$HOME/Library/Application Support/bmux" ;;
        MINGW* | MSYS* | CYGWIN*) printf '%s\n' "$(cygpath -u "$APPDATA")/bmux" ;;
        *) printf '%s\n' "${XDG_DATA_HOME:-$HOME/.local/share}/bmux" ;;
    esac
}

case "$(uname -s)" in
    Darwin) library=libbclean_bmux_plugin.dylib ;;
    MINGW* | MSYS* | CYGWIN*) library=bclean_bmux_plugin.dll ;;
    *) library=libbclean_bmux_plugin.so ;;
esac

install_dir="$(data_dir)/plugins/bclean"
if [[ "$print_path" == 1 ]]; then
    printf '%s\n' "$install_dir"
    exit 0
fi

target_dir="${CARGO_TARGET_DIR:-$root/target}"
if [[ "$profile" == release ]]; then
    cargo build --release -p bclean_bmux_plugin --manifest-path "$root/Cargo.toml"
else
    cargo build -p bclean_bmux_plugin --manifest-path "$root/Cargo.toml"
fi
lib_path="$target_dir/$profile/$library"
[[ -f "$lib_path" ]] || {
    echo "built library not found at $lib_path" >&2
    exit 1
}

manifest="$install_dir/plugin.toml"
if [[ -e "$manifest" && "$force" != 1 ]]; then
    echo "manifest already exists at $manifest (use --force to replace it)" >&2
    exit 1
fi
mkdir -p "$install_dir"
{
    printf 'entry = "%s"\n' "$lib_path"
    grep -v '^#' "$plugin_dir/plugin.toml"
} >"$manifest"
echo "installed $manifest"
echo "enable it with:  [plugins] enabled = [\"bclean\"]  in your bmux config"
