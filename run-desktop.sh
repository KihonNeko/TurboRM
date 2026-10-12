#!/bin/sh
set -eu
project_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
if command -v cargo >/dev/null 2>&1; then
    cargo build --release --bins --manifest-path "$project_dir/turborm/Cargo.toml" --locked
fi
# One Slint executable: library, pairing and viewer live in the same window.
for profile in release debug; do
    executable="$project_dir/target/$profile/turborm"
    if [ -x "$executable" ]; then
        exec "$executable" "$@"
    fi
done
printf '%s\n' 'Build TurboRM with Rust first. See turborm/README.md.' >&2
exit 1
