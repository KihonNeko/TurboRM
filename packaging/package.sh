#!/bin/sh
# Pack already-built release binaries into the download attached to a GitHub
# release, with its SHA-256 checksum.
#
#   packaging/package.sh <version> [out-dir]
#
# Expects target/release/turborm and target/release/turborm-cli, and
# THIRD-PARTY-LICENSES.html at the repository root (made with cargo about).
set -eu

[ $# -ge 1 ] && [ $# -le 2 ] || { echo "Usage: $0 <version> [out-dir]" >&2; exit 2; }
version=$1
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
mkdir -p "${2:-$root/dist}"
out=$(CDPATH= cd -- "${2:-$root/dist}" && pwd)
name=turborm-$version-$(uname -m)-linux

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
dir=$stage/$name
mkdir -p "$dir/icons"

cd "$root"
install -m755 target/release/turborm target/release/turborm-cli packaging/install.sh "$dir/"
install -m644 README.md LICENSE THIRD-PARTY-LICENSES.html turborm/turborm.desktop "$dir/"
install -m644 turborm/fonts/Inter-LICENSE.txt "$dir/Inter-LICENSE.txt"
install -m644 turborm/icons/turborm.svg turborm/icons/turborm.png \
    turborm/icons/turborm-16.png turborm/icons/turborm-22.png turborm/icons/turborm-24.png \
    "$dir/icons/"

tar -C "$stage" --owner=0 --group=0 -czf "$out/$name.tar.gz" "$name"
(cd "$out" && sha256sum "$name.tar.gz" >"$name.tar.gz.sha256")
echo "$out/$name.tar.gz"
