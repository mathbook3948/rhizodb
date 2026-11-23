#!/usr/bin/env bash
set -euo pipefail

commit=4c04eea0bae3c8be9e9a4ff05dd19d4403add23a
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(git -C "$here" rev-parse --show-toplevel)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

git -C "$root" archive "$commit" Cargo.toml Cargo.lock crates/rhizodb | tar -x -C "$work"
mkdir -p "$work/crates/format-7/src"
cp "$here/format-7.rs" "$work/crates/format-7/src/main.rs"
cat > "$work/crates/format-7/Cargo.toml" <<'EOF'
[package]
name = "format-7"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
rhizodb = { path = "../rhizodb" }
redb = "=5.0.0"

[lints]
workspace = true
EOF

for fixture in basic wide; do
  case "$fixture" in
    basic) out="$here/format-7.redb" ;;
    wide) out="$here/format-7-wide.redb" ;;
  esac
  rm -f "$out"
  echo "== $fixture: $out"
  cargo run --quiet --release --offline --manifest-path "$work/Cargo.toml" --package format-7 -- "$fixture" "$out"
done
