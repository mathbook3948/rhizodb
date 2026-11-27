#!/usr/bin/env bash
set -euo pipefail

commit=10a17f222dd037f531a796d17fdea784df5089ad
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(git -C "$here" rev-parse --show-toplevel)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

git -C "$root" archive "$commit" Cargo.toml Cargo.lock crates/rhizodb | tar -x -C "$work"
mkdir -p "$work/crates/format-14/src"
cp "$here/format-14.rs" "$work/crates/format-14/src/main.rs"
cat > "$work/crates/format-14/Cargo.toml" <<'TOML'
[package]
name = "format-14"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
rhizodb = { path = "../rhizodb" }

[lints]
workspace = true
TOML

out="$here/format-14.db"
rm -f "$out"
cargo run --quiet --release --offline --manifest-path "$work/Cargo.toml" --package format-14 -- "$out" > "$here/format-14.answers"
