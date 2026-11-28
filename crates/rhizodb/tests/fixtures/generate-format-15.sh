#!/usr/bin/env bash
set -euo pipefail

commit=b95948b8661c23fa63f8201af4c3c2eb1d2b1450
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(git -C "$here" rev-parse --show-toplevel)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

git -C "$root" archive "$commit" Cargo.toml Cargo.lock crates/rhizodb | tar -x -C "$work"
mkdir -p "$work/crates/format-15/src"
cp "$here/format-15.rs" "$work/crates/format-15/src/main.rs"
cat > "$work/crates/format-15/Cargo.toml" <<'TOML'
[package]
name = "format-15"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
rhizodb = { path = "../rhizodb" }

[lints]
workspace = true
TOML

out="$here/format-15.db"
rm -f "$out"
cargo run --quiet --release --offline --manifest-path "$work/Cargo.toml" --package format-15 -- "$out" > "$here/format-15.answers"
