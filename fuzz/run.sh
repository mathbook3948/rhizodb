#!/bin/sh
set -eu

seconds="${1:-300}"
if [ "$#" -gt 0 ]; then
  shift
fi
targets="${*:-parse value_decode graph_ops db_file engine_formats engine_file replication_changes snapshot_install dump_import engine_resealed graph_resealed parameters}"
toolchain="nightly-2026-11-20"
cd "$(dirname "$0")"
limits="-timeout=20 -rss_limit_mb=3072"

rustup toolchain install "$toolchain" --profile minimal
cargo "+$toolchain" metadata --locked --format-version 1 > /dev/null
for target in $targets; do
  cargo "+$toolchain" fuzz run "$target" -- -max_total_time="$seconds" -max_len=1024 $limits
done
