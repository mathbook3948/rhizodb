#!/usr/bin/env bash
set -euo pipefail

commit=84cfd09c2a05940b1efd7c1491b1db362946805b
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(git -C "$here" rev-parse --show-toplevel)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

git -C "$root" archive "$commit" Cargo.toml Cargo.lock crates/rhizodb | tar -x -C "$work"
mkdir -p "$work/crates/engine-1-1/src"
cp "$here/engine-1-1.rs" "$work/crates/engine-1-1/src/main.rs"
cat > "$work/crates/engine-1-1/Cargo.toml" <<'TOML'
[package]
name = "engine-1-1"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
rhizodb = { path = "../rhizodb" }

[lints]
workspace = true
TOML

out="$here/engine-1-1.rhizodb"
rm -f "$out"
cargo run --quiet --release --offline --manifest-path "$work/Cargo.toml" --package engine-1-1 -- "$out" > "$here/engine-1-1.answers"
