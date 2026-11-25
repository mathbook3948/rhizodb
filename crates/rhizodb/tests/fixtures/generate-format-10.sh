#!/usr/bin/env bash
set -euo pipefail

commit=edb90b32cc2bdbd601619a6c991560e2fd4a7db9
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(git -C "$here" rev-parse --show-toplevel)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

git -C "$root" archive "$commit" Cargo.toml Cargo.lock crates/rhizodb | tar -x -C "$work"
mkdir -p "$work/crates/format-10/src"
cp "$here/format-10.rs" "$work/crates/format-10/src/main.rs"
cat > "$work/crates/format-10/Cargo.toml" <<'TOML'
[package]
name = "format-10"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
rhizodb = { path = "../rhizodb" }
redb = "=5.0.0"

[lints]
workspace = true
TOML

out="$here/format-10.redb"
rm -f "$out"
cargo run --quiet --release --offline --manifest-path "$work/Cargo.toml" --package format-10 -- "$out" > "$here/format-10.answers"
