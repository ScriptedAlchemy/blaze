#!/usr/bin/env bash
set -euo pipefail

pin="$GITHUB_WORKSPACE/pins/clang.json"
target=aarch64-unknown-linux-gnu
shelf=${1:?expected the toolchain shelf directory}
mkdir -p "$shelf/clang" "$shelf/sysroot"
stage=$(mktemp -d "$shelf/.linux-c.XXXXXXXX")
trap 'rm -rf -- "$stage"' EXIT
mkdir -p "$stage/clang/runtime" "$stage/headers"

# All archives are authenticated before either extractor sees them. dpkg-deb -x
# only unpacks package data, so no package maintainer scripts execute.
fetch() {
  local url=$1 size=$2 digest=$3 dest=$4
  curl --fail --silent --show-error --location --proto '=https' --proto-redir '=https' \
    --max-time 420 --max-filesize "$size" --output "$dest" "$url"
  test "$(stat -c %s "$dest")" = "$size"
  printf '%s  %s\n' "$digest" "$dest" | sha256sum --check
}

archive=$(jq -er --arg t "$target" '.targets[$t].name' "$pin")
clang_id=$(jq -er --arg t "$target" '.targets[$t].sha256' "$pin")
sdk_id=$(jq -er --arg t "$target" '.sysroots[$t]' "$pin")
size=$(jq -er --arg t "$target" '.targets[$t].size' "$pin")
fetch "$(jq -er '.release' "$pin")/$archive" "$size" "$clang_id" "$stage/llvm.tar.xz"
top=${archive%.tar.xz}
major=$(jq -er '.version | split(".")[0]' "$pin")
tar -xJf "$stage/llvm.tar.xz" --no-same-owner -C "$stage/clang" --strip-components=1 \
  "$top/bin/clang" "$top/bin/clang++" "$top/bin/clang-$major" "$top/bin/llvm-ar" \
  "$top/bin/llvm-nm" "$top/lib/clang/$major/include"

jq -cer '.linux_c_inputs | (.headers + .runtime)[]' "$pin" > "$stage/packages"
while IFS= read -r row; do
  fetch "$(jq -er '.url' <<< "$row")" "$(jq -er '.size' <<< "$row")" \
    "$(jq -er '.sha256' <<< "$row")" "$stage/package.deb"
  if jq -e 'has("libraries")' <<< "$row" > /dev/null; then
    dpkg-deb -x "$stage/package.deb" "$stage/host"
    jq -er '.libraries[]' <<< "$row" > "$stage/libraries"
    while IFS= read -r path; do
      install -m 0555 "$stage/host/$path" "$stage/clang/runtime/${path##*/}"
    done < "$stage/libraries"
  else
    dpkg-deb --fsys-tarfile "$stage/package.deb" | tar -x --no-same-owner -C "$stage/headers" ./usr/include
  fi
done < "$stage/packages"

# The compiler validates both complete tree digests before its first C cache lookup.
# Stage on the destination filesystem so each published directory is complete.
publish() {
  local source=$1 destination=$2
  if mv -T -- "$source" "$destination"; then
    return
  fi
  # A previous invocation may already have installed this pinned directory.
  test -d "$destination" && test ! -L "$destination"
  diff -qr --no-dereference "$source" "$destination"
}
publish "$stage/headers" "$shelf/sysroot/$sdk_id"
publish "$stage/clang" "$shelf/clang/$clang_id"
