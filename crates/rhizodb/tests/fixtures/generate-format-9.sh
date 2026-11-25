#!/usr/bin/env bash
set -euo pipefail

commit=e519889eaadfef5c3a17ca4b5cf90b2c6a7b201d
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(git -C "$here" rev-parse --show-toplevel)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

git -C "$root" archive "$commit" Cargo.toml Cargo.lock crates/rhizodb | tar -x -C "$work"
mkdir -p "$work/crates/format-9/src"
cp "$here/format-9.rs" "$work/crates/format-9/src/main.rs"
cat > "$work/crates/format-9/Cargo.toml" <<'TOML'
[package]
name = "format-9"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
rhizodb = { path = "../rhizodb" }
redb = "=5.0.0"

[lints]
workspace = true
TOML

out="$here/format-9.redb"
rm -f "$out"
cargo run --quiet --release --offline --manifest-path "$work/Cargo.toml" --package format-9 -- "$out" > "$here/format-9.answers"
