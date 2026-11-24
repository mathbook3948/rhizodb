#!/bin/sh
set -eu

commit="${1:?usage: sync-kuzu.sh <commit>}"
vendor="$(cd "$(dirname "$0")" && pwd)/vendor/kuzu"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

git clone --quiet --filter=blob:none --no-checkout https://github.com/kuzudb/kuzu.git "$work/src"
git -C "$work/src" checkout --quiet "$commit" -- LICENSE test/test_files

find "$vendor" -mindepth 1 -delete
mkdir -p "$vendor/test_files"
cp "$work/src/LICENSE" "$vendor/"
cd "$work/src/test/test_files"
for directory in transaction dml_node dml_rel function/utility projection common/types recursive_join issue; do
  grep -rlix -e '-DATASET CSV empty' "$directory" | while read -r file; do
    mkdir -p "$vendor/test_files/$(dirname "$file")"
    cp "$file" "$vendor/test_files/$file"
  done
done
printf '%s\n%s\n' "https://github.com/kuzudb/kuzu.git" "$(git -C "$work/src" rev-parse HEAD)" > "$vendor/SOURCE"
