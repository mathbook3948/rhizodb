#!/bin/sh
set -eu

commit="${1:?usage: sync.sh <commit>}"
vendor="$(cd "$(dirname "$0")" && pwd)/vendor/opencypher"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

git clone --quiet https://github.com/opencypher/openCypher.git "$work/src"
git -C "$work/src" checkout --quiet "$commit"

find "$vendor" -mindepth 1 -delete
cp -R "$work/src/tck/features" "$work/src/tck/graphs" "$work/src/LICENSE" "$vendor/"
printf '%s\n%s\n' "https://github.com/opencypher/openCypher.git" "$(git -C "$work/src" rev-parse HEAD)" > "$vendor/SOURCE"
