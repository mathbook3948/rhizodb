#!/usr/bin/env bash
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
scale="${1:-medium}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

cargo run --release --quiet --manifest-path "$here/Cargo.toml" --example load_file -- "$scale" "$work"
RHIZODB_SIZE_FILE="$work/graph/graph.rhizodb" cargo test --release --quiet \
  --manifest-path "$here/../../Cargo.toml" --package rhizodb --lib engine::size_report \
  -- --ignored --nocapture
