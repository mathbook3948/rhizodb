#!/usr/bin/env bash
set -euo pipefail

commit=854bca892181a4ba60171d7b3dd6a4c2bba120b7
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(git -C "$here" rev-parse --show-toplevel)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

git -C "$root" archive "$commit" Cargo.toml Cargo.lock crates/rhizodb | tar -x -C "$work"
mkdir -p "$work/crates/engine-1-1-holes/src"
cp "$here/engine-1-1-holes.rs" "$work/crates/engine-1-1-holes/src/main.rs"
cat > "$work/crates/engine-1-1-holes/Cargo.toml" <<'TOML'
[package]
name = "engine-1-1-holes"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
rhizodb = { path = "../rhizodb" }

[lints]
workspace = true
TOML

out="$here/engine-1-1-holes.rhizodb"
rm -f "$out"
cargo run --quiet --release --offline --manifest-path "$work/Cargo.toml" --package engine-1-1-holes -- "$out" > "$here/engine-1-1-holes.answers"
