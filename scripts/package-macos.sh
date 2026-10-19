#!/usr/bin/env bash
# Run from the repository root after cargo build --release --locked.
set -euo pipefail

binary=${1:-target/release/glimcode}
output=${2:-dist}
arch=$(lipo -archs "$binary")
case "$arch" in
  arm64|x86_64) ;;
  *) echo "Expected an arm64 or x86_64 macOS binary, got: $arch" >&2; exit 1 ;;
esac

mkdir -p "$output"
staging=$(mktemp -d)
trap 'rm -rf "$staging"' EXIT
name="glimcode-macos-$arch"
mkdir -p "$staging/$name"
install -m 755 "$binary" "$staging/$name/glimcode"
cp LICENSE README.md "$staging/$name/"
cp -R docs "$staging/$name/docs"
tar -czf "$output/$name.tar.gz" -C "$staging" "$name"
# Check that the downloadable archive is readable and includes the executable.
tar -tzf "$output/$name.tar.gz" | grep -Fx "$name/glimcode"
echo "Packaged $output/$name.tar.gz"
