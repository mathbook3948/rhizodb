#!/usr/bin/env bash
set -euo pipefail

commit=84cfd09c2a05940b1efd7c1491b1db362946805b
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(git -C "$here" rev-parse --show-toplevel)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

git -C "$root" archive "$commit" Cargo.toml Cargo.lock crates/rhizodb | tar -x -C "$work"
mkdir -p "$work/crates/format-13/src"
cp "$here/format-13.rs" "$work/crates/format-13/src/main.rs"
cat > "$work/crates/format-13/Cargo.toml" <<'TOML'
[package]
name = "format-13"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
rhizodb = { path = "../rhizodb" }

[lints]
workspace = true
TOML

out="$here/format-13.db"
rm -f "$out"
cargo run --quiet --release --offline --manifest-path "$work/Cargo.toml" --package format-13 -- "$out" > "$here/format-13.answers"
