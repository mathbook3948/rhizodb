#!/usr/bin/env bash
set -euo pipefail

commit=a59879110e2885a45f946c4549a74c5e2cae9d43
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(git -C "$here" rev-parse --show-toplevel)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

git -C "$root" archive "$commit" Cargo.toml Cargo.lock crates/rhizodb | tar -x -C "$work"
mkdir -p "$work/crates/format-12/src"
cp "$here/format-12.rs" "$work/crates/format-12/src/main.rs"
cat > "$work/crates/format-12/Cargo.toml" <<'TOML'
[package]
name = "format-12"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
rhizodb = { path = "../rhizodb" }
redb = "=5.0.0"

[lints]
workspace = true
TOML

out="$here/format-12.redb"
rm -f "$out"
cargo run --quiet --release --offline --manifest-path "$work/Cargo.toml" --package format-12 -- "$out" > "$here/format-12.answers"
