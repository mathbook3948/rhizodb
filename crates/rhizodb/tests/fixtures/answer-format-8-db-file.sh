#!/usr/bin/env bash
set -euo pipefail

commit=765d472ee36cec43a4cadbd6a73124dab2b63833
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(git -C "$here" rev-parse --show-toplevel)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

git -C "$root" archive "$commit" Cargo.toml Cargo.lock crates/rhizodb | tar -x -C "$work"
mkdir -p "$work/crates/format-8/src"
cp "$here/format-8.rs" "$work/crates/format-8/src/main.rs"
cat > "$work/crates/format-8/Cargo.toml" <<'TOML'
[package]
name = "format-8"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
rhizodb = { path = "../rhizodb" }
redb = "=5.0.0"

[lints]
workspace = true
TOML

cp "$here/format-8-db-file.redb" "$work/graph.redb"
cargo run --quiet --release --offline --manifest-path "$work/Cargo.toml" --package format-8 -- answer-db-file "$work/graph.redb" > "$here/format-8-db-file.answers"
