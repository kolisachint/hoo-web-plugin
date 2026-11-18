#!/usr/bin/env bash
# Build the webtools extension component into plugin/dev.hoocode/extension.wasm.
#
# The component is its own Cargo workspace (plugin/component, excluded from the
# root). Its path dependencies (crates/) sit outside that workspace, and cargo
# hashes their absolute path into the crate metadata: symbol names and code
# layout then differ between checkout paths. So the sources are copied to one
# fixed staging path and built there. Paths in the output are remapped as well:
# the staging tree, the target dir, the cargo home and the rustup sysroot.
# The target dir stays outside the repository (cargo writes a relative OUT_DIR
# for a target dir inside the workspace, and that string is not remapped).
#
# The same sources give the same bytes from any checkout path and any target
# dir. Set WEBTOOLS_TARGET_DIR to choose the target dir.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="$root/plugin/dev.hoocode/extension.wasm"
target_dir="${WEBTOOLS_TARGET_DIR:-${TMPDIR:-/tmp}/webtools-plugin-target}"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
sysroot="$(rustc --print sysroot)"
# Fixed on purpose: the metadata hash depends on it. Do not make it configurable.
stage="/tmp/webtools-plugin-src"
lock="$stage.lock"
case "$target_dir" in
    "$root"|"$root"/*) echo "build.sh: target dir must be outside $root" >&2; exit 1 ;;
esac

# /tmp is shared on some hosts. Serialise builds with a lock where flock exists;
# without it, concurrent builds on one host are unsupported.
if command -v flock >/dev/null 2>&1; then
    if [ -L "$lock" ]; then echo "build.sh: $lock is a symlink" >&2; exit 1; fi
    exec 9>"$lock"
    flock 9
else
    echo "build.sh: flock not found; concurrent builds on this host are unsupported" >&2
fi

rm -rf "$stage"
# No -p: mkdir fails if something appeared at the stage path since the rm above,
# rather than building into a directory someone else controls.
mkdir "$stage"
trap 'rm -rf "$stage"' EXIT
chmod 700 "$stage"
if [ -z "$(find "$stage" -maxdepth 0 -user "$(id -u)" -print)" ] \
    || [ -n "$(find "$stage" -maxdepth 0 -perm -022 -print)" ]; then
    echo "build.sh: $stage is not owned by you or is group/world-writable" >&2
    exit 1
fi
tar -C "$root" --exclude=.git --exclude=target --exclude='target-*' \
    --exclude=component/target -cf - . | tar -C "$stage" -xf -

export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$stage=. --remap-path-prefix=$target_dir=/target --remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$sysroot=/rustup"

cargo build \
    --manifest-path "$stage/plugin/component/Cargo.toml" \
    --release \
    --locked \
    --target wasm32-wasip2 \
    --target-dir "$target_dir"

mkdir -p "$(dirname "$out")"
cp "$target_dir/wasm32-wasip2/release/webtools_extension.wasm" "$out"
echo "wrote $out ($(wc -c < "$out") bytes)"
