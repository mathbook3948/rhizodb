#!/usr/bin/env bash
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
report="${BENCH_README_REPORT:-}"
if [[ -z "$report" ]]; then
  report="$here/target/readme-bench.json"
  cargo run --release --manifest-path "$here/Cargo.toml" --bin bench -- \
    --scale medium --reps 5 --engines rhizodb,ladybugdb --out "$report"
fi

cpu="$(lscpu 2>/dev/null | sed -n 's/^Model name: *//p' | head -n 1)"
if [[ -z "$cpu" ]]; then
  cpu="$(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo "unknown CPU")"
fi
cores="$(getconf _NPROCESSORS_ONLN)"
machine="$cores CPUs ($cpu), $(uname -sr)"
date="$(date -u -r "$report" +%Y-%m-%d 2>/dev/null || date -u +%Y-%m-%d)"

cargo run --release --quiet --manifest-path "$here/Cargo.toml" --bin readme-table -- \
  "$report" "$here/../../README.md" "$machine" "$date"
echo "rewrote the benchmark table in README.md from $report"
