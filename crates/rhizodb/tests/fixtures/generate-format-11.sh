#!/usr/bin/env bash
set -euo pipefail

commit=8a8b4956ce72c42e984e3499e0a1d9942396b5a9
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(git -C "$here" rev-parse --show-toplevel)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

git -C "$root" archive "$commit" Cargo.toml Cargo.lock crates/rhizodb | tar -x -C "$work"
mkdir -p "$work/crates/format-11/src"
cp "$here/format-11.rs" "$work/crates/format-11/src/main.rs"
cat > "$work/crates/format-11/Cargo.toml" <<'TOML'
[package]
name = "format-11"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
rhizodb = { path = "../rhizodb" }
redb = "=5.0.0"

[lints]
workspace = true
TOML

out="$here/format-11.redb"
rm -f "$out"
cargo run --quiet --release --offline --manifest-path "$work/Cargo.toml" --package format-11 -- "$out" > "$here/format-11.answers"
