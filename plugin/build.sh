#!/usr/bin/env bash
# Build the webtools extension component into plugin/dev.hoocode/extension.wasm.
#
# The component is its own Cargo workspace (plugin/component, excluded from the
# root). Paths are remapped (panic messages and build-script paths otherwise embed
# the checkout and the cargo home), and the target directory is kept outside the
# repository: cargo writes a relative OUT_DIR for a target dir inside the
# workspace, and that string is not remapped. With this, the same checkout gives
# the same bytes across target directories. Different checkout paths still give
# a few bytes of difference (not yet traced). Set WEBTOOLS_TARGET_DIR to override.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
component="$root/plugin/component"
out="$root/plugin/dev.hoocode/extension.wasm"
target_dir="${WEBTOOLS_TARGET_DIR:-${TMPDIR:-/tmp}/webtools-plugin-target}"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
case "$target_dir" in
    "$root"|"$root"/*) echo "build.sh: target dir must be outside $root" >&2; exit 1 ;;
esac

export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$target_dir=/target --remap-path-prefix=$root=. --remap-path-prefix=$cargo_home=/cargo"

cargo build \
    --manifest-path "$component/Cargo.toml" \
    --release \
    --target wasm32-wasip2 \
    --target-dir "$target_dir"

mkdir -p "$(dirname "$out")"
cp "$target_dir/wasm32-wasip2/release/webtools_extension.wasm" "$out"
echo "wrote $out ($(wc -c < "$out") bytes)"
