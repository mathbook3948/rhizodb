#!/bin/sh
set -eu

seconds="${1:-300}"
cd "$(dirname "$0")"
limits="-timeout=20 -rss_limit_mb=3072"

for target in parse value_decode graph_ops db_file; do
  cargo +nightly fuzz run "$target" -- -max_total_time="$seconds" -max_len=1024 $limits
done
