#!/usr/bin/env bash
set -euo pipefail

tag=v0.1.0
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(git -C "$here" rev-parse --show-toplevel)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

git -C "$root" archive "$tag" Cargo.toml Cargo.lock crates/rhizodb | tar -x -C "$work"
mkdir -p "$work/crates/release-0-1-0/src"
cp "$here/release-0-1-0.rs" "$work/crates/release-0-1-0/src/main.rs"
cat > "$work/crates/release-0-1-0/Cargo.toml" <<'TOML'
[package]
name = "release-0-1-0"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
rhizodb = { path = "../rhizodb" }

[lints]
workspace = true
TOML

cargo build --quiet --release --offline --manifest-path "$work/Cargo.toml" --package release-0-1-0
binary="$(cargo metadata --format-version 1 --no-deps --manifest-path "$work/Cargo.toml" | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/release/release-0-1-0"

for mode in clean crashed; do
  base="$here/release-0-1-0-$mode"
  rm -f "$base".rhizodb "$base".rhizodb-wal-* "$base".dump "$base".answers
  status=0
  "$binary" "$mode" "$base.rhizodb" "$base.dump" > "$base.answers" || status=$?
  case "$mode:$status" in
    clean:0 | crashed:134) ;;
    *) echo "release-0-1-0 $mode exited with $status" >&2; exit 1 ;;
  esac
done
