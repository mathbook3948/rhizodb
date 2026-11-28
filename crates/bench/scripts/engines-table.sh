#!/usr/bin/env bash
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
report="${BENCH_ENGINES_REPORT:-}"
if [[ -z "$report" ]]; then
  report="$here/target/engines-bench.json"
  servers=()
  [[ -n "${BENCH_NEO4J_URL:-}" ]] && servers+=(--neo4j "$BENCH_NEO4J_URL")
  [[ -n "${BENCH_MEMGRAPH_URL:-}" ]] && servers+=(--memgraph "$BENCH_MEMGRAPH_URL")
  [[ -n "${BENCH_FALKORDB_URL:-}" ]] && servers+=(--falkordb "$BENCH_FALKORDB_URL")
  cargo run --release --features duckdb,cozo --manifest-path "$here/Cargo.toml" --bin bench -- \
    --scale medium --reps 5 --skip "${BENCH_ENGINES_SKIP-index/vector}" --out "$report" "${servers[@]}"
fi

cpu="$(lscpu 2>/dev/null | sed -n 's/^Model name: *//p' | head -n 1)"
if [[ -z "$cpu" ]]; then
  cpu="$(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo "unknown CPU")"
fi
cores="$(getconf _NPROCESSORS_ONLN)"
machine="$cores CPUs ($cpu), $(uname -sr)"
date="$(date -u -r "$report" +%Y-%m-%d 2>/dev/null || date -u +%Y-%m-%d)"

cargo run --release --quiet --features duckdb,cozo --manifest-path "$here/Cargo.toml" \
  --bin engines-table -- "$report" "$here/ENGINES.md" "$machine" "$date"
echo "rewrote the table in crates/bench/ENGINES.md from $report"
