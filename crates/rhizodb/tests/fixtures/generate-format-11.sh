#!/usr/bin/env bash
set -euo pipefail

commit=74d59b39e7baf686141c62a4d92253e10dec4e65
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

[lints]
workspace = true
TOML

out="$here/format-11.db"
rm -f "$out"
cargo run --quiet --release --offline --manifest-path "$work/Cargo.toml" --package format-11 -- "$out" > "$here/format-11.answers"
