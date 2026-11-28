#!/usr/bin/env bash
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="$(sed -n 's/^lbug = "=\(.*\)"/\1/p' "$here/Cargo.toml")"
out="${LBUG_EXTENSION_DIR:-$here/.lbug-extensions}"
work="$out/work"
jobs="${LBUG_EXTENSION_JOBS:-$(nproc)}"

if [[ -f "$out/fts/libfts.lbug_extension" && -f "$out/vector/libvector.lbug_extension" && -f "$out/version" && "$(cat "$out/version")" == "$version" ]]; then
  echo "extensions for lbug $version are already built in $out"
  exit 0
fi

mkdir -p "$work"
if [[ ! -d "$work/ladybug/.git" ]]; then
  git clone --quiet -c core.autocrlf=false --depth 1 --branch "v$version" https://github.com/LadybugDB/ladybug "$work/ladybug"
fi
commit="$(git -C "$work/ladybug" ls-tree HEAD extension | awk '{print $3}')"
if [[ ! -d "$work/extensions/.git" ]]; then
  git clone --quiet -c core.autocrlf=false https://github.com/ladybugdb/extensions.git "$work/extensions"
fi
git -C "$work/extensions" checkout --quiet "$commit"
rm -rf "$work/ladybug/extension"
ln -s "$work/extensions" "$work/ladybug/extension"

crate="$(ls -d "${CARGO_HOME:-$HOME/.cargo}"/registry/src/*/lbug-"$version"/lbug-src | head -n 1)"
diff -rq "$work/ladybug/src" "$crate/src" > /dev/null || {
  echo "the engine sources of tag v$version differ from the lbug crate, refusing to build extensions" >&2
  exit 1
}

cmake -S "$work/ladybug" -B "$work/build" -DCMAKE_BUILD_TYPE=Release -DBUILD_EXTENSIONS="fts;vector" \
  -DBUILD_SHELL=FALSE -DBUILD_TESTS=FALSE -DBUILD_EXAMPLES=FALSE -DBUILD_BENCHMARK=FALSE > /dev/null
cmake --build "$work/build" --config Release --target lbug_fts_extension lbug_vector_extension -j "$jobs"

for name in fts vector; do
  mkdir -p "$out/$name"
  built="$work/ladybug/extension/$name/build/lib$name.lbug_extension"
  if [[ ! -f "$built" ]]; then
    built="$work/ladybug/extension/$name/build/Release/lib$name.lbug_extension"
  fi
  cp "$built" "$out/$name/"
done
echo "$version" > "$out/version"
echo "built the extensions for lbug $version from extensions commit $commit into $out"
