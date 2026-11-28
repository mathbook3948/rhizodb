#!/usr/bin/env bash
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export LBUG_VERSION="$(sed -n 's/^lbug = "=\(.*\)"/\1/p' "$here/Cargo.toml")"
report="${BENCH_README_REPORT:-}"
if [[ -z "$report" ]]; then
  report="$here/target/readme-bench.json"
  cargo run --release --manifest-path "$here/Cargo.toml" --bin bench -- \
    --scale medium --reps 5 --engines rhizodb,ladybugdb --out "$report"
fi

cpu="$(lscpu 2>/dev/null | sed -n 's/^Model name: *//p' | head -n 1 || true)"
if [[ -z "$cpu" ]]; then
  cpu="$(sysctl -n machdep.cpu.brand_string 2>/dev/null || true)"
fi
system="$(uname -sr)"
if [[ "$(uname -s)" == MINGW* || "$(uname -s)" == MSYS* ]]; then
  if [[ -z "$cpu" ]]; then
    cpu="$(powershell.exe -NoProfile -Command '(Get-CimInstance Win32_Processor).Name' | tr -d '\r')"
  fi
  system="Windows $(powershell.exe -NoProfile -Command '[Environment]::OSVersion.Version.ToString()' | tr -d '\r')"
fi
cores="$(getconf _NPROCESSORS_ONLN)"
machine="$cores CPUs (${cpu:-unknown CPU}), $system"
date="$(date -u -r "$report" +%Y-%m-%d 2>/dev/null || date -u +%Y-%m-%d)"

cargo run --release --quiet --manifest-path "$here/Cargo.toml" --bin readme-table -- \
  "$report" "$here/../../README.md" "$machine" "$date"
cargo run --release --quiet --manifest-path "$here/Cargo.toml" --bin readme-table -- \
  "$report" "$here/../../README.ko.md" "$machine" "$date" ko
cp "$here/../../README.md" "$here/../rhizodb/README.md"
echo "rewrote the benchmark tables in README.md, README.ko.md, and the copy in crates/rhizodb from $report"
