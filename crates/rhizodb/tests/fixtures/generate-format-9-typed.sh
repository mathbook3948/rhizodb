#!/usr/bin/env bash
set -euo pipefail

commit=77cffd2224a4820ff0c6a790c50617fc55e7e30f
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(git -C "$here" rev-parse --show-toplevel)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

git -C "$root" archive "$commit" Cargo.toml Cargo.lock crates/rhizodb | tar -x -C "$work"
mkdir -p "$work/crates/format-9-typed/src"
cp "$here/format-9-typed.rs" "$work/crates/format-9-typed/src/main.rs"
cat > "$work/crates/format-9-typed/Cargo.toml" <<'EOF'
[package]
name = "format-9-typed"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
rhizodb = { path = "../rhizodb" }
redb = "=5.0.0"

[lints]
workspace = true
EOF

out="$here/format-9-typed.redb"
rm -f "$out"
cargo run --quiet --release --offline --manifest-path "$work/Cargo.toml" --package format-9-typed -- "$out" > "$here/format-9-typed.answers"
