#!/bin/sh
set -eu

seconds="${1:-300}"
cd "$(dirname "$0")"
limits="-timeout=20 -rss_limit_mb=3072"

for target in parse value_decode graph_ops; do
  cargo +nightly fuzz run "$target" -- -max_total_time="$seconds" -max_len=1024 $limits
done

triage="artifacts/db_file_triage"
mkdir -p "$triage"
cargo +nightly fuzz run db_file -- -max_total_time="$seconds" -fork=1 -ignore_crashes=1 \
  -ignore_timeouts=0 -ignore_ooms=0 -artifact_prefix="$triage/" $limits || true

failed=0
for input in "$triage"/*; do
  [ -e "$input" ] || continue
  case "$(basename "$input")" in
    crash-*) ;;
    *)
      echo "db_file: $input is a hang or an out-of-memory"
      failed=1
      continue
      ;;
  esac
  location="$(cargo +nightly fuzz run db_file "$input" 2>&1 | grep -m1 'panicked at' || true)"
  case "$location" in
    */redb-*) echo "db_file: known redb panic, $location" ;;
    *)
      echo "db_file: $input panics outside redb: $location"
      failed=1
      ;;
  esac
done
exit "$failed"
