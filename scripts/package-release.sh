#!/usr/bin/env bash
# Build and package a wallbreak release tarball for x86_64 Linux.
#
# Usage: scripts/package-release.sh [output-dir]
#
# The tarball contains both binaries plus README and LICENSE. It does not
# bundle FFmpeg or OpenGL/Wayland runtime libraries; the Nix flake remains the
# recommended installation because it supplies those dependencies.
set -euo pipefail

root="$(dirname "$(dirname "$(realpath "$0")")")"
out="${1:-$root/dist}"
version="$(grep -m1 '^version' "$root/Cargo.toml" | cut -d'"' -f2)"
test -n "$version"

cargo build --locked --release --features gui --manifest-path "$root/Cargo.toml"

stage="$out/stage/wallbreak-$version-x86_64-linux"
rm -rf "$stage"
mkdir -p "$stage"
cp "$root/target/release/wallbreak" "$stage/"
cp "$root/target/release/wallbreak-gui" "$stage/"
cp "$root/README.md" "$root/LICENSE" "$stage/"

archive="$out/wallbreak-$version-x86_64-linux.tar.gz"
tar -czf "$archive" -C "$out/stage" "wallbreak-$version-x86_64-linux"
sha256sum "$archive" | awk '{print $1}' > "$archive.sha256"

echo "wrote $archive"
cat "$archive.sha256"
