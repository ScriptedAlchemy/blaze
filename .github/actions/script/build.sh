#!/usr/bin/env bash
# build.sh DIR KEY builds every scripts/src/bin/<name>.rs with pins/toolchain.
# key.sh binds the complete source dependency closure; the compiler-artifact records
# name exactly the executables copied into the atomically replaced cache directory.
set -euo pipefail
die() { echo "::error::$*" >&2; echo "build.sh: $*" >&2; exit 1; }
[ $# = 2 ] || die "takes DIR and KEY, not: $*"
dir=$1 key=$2

tc=$(cat pins/toolchain)
cargo +"$tc" --version >/dev/null 2>&1 || die "no cargo +$tc on PATH (pins/toolchain names $tc)"

# What Cargo reads from the caller's environment, so
# the key, which names none of it, covers the build.
unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS RUSTDOCFLAGS RUSTC RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER \
  CARGO_BUILD_TARGET CARGO_BUILD_JOBS CARGO_INCREMENTAL BLAZE_BACKEND BLAZE_OPT BLAZE_EMIT
for name in $(env | sed -nE 's/^(CARGO_PROFILE_[A-Z0-9_]*|CARGO_TARGET_[A-Z0-9_]*)=.*/\1/p'); do unset "$name"; done

unset CARGO_TARGET_DIR
said="$dir.build.json"
mkdir -p "$(dirname "$dir")"
if ! timeout -k 30 1200 cargo +"$tc" build --locked -p scripts --bins --message-format=json > "$said"; then
  grep '"level":"error"' "$said" | sed -n 's/.*"rendered":"\([^"]*\)".*/\1/p' | sed 's/\\n/\n/g' >&2 || true
  die "cargo +$tc build --locked -p scripts --bins --message-format=json failed (its JSON lines: $said)"
fi

fresh="$dir.new"
rm -rf "$fresh"
mkdir -p "$fresh"
for src in scripts/src/bin/*.rs; do
  name=$(basename "$src" .rs)
  # The executables the build names whose file is `<name>`: exactly one.
  found=$(grep '"reason":"compiler-artifact"' "$said" | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | awk -v n="$name" -F/ '$NF == n' | sort -u)
  [ -n "$found" ] || die "cargo build -p scripts --bins named no executable \`$name\` in a compiler-artifact line ($said)"
  [ "$(echo "$found" | wc -l)" = 1 ] || die "cargo build -p scripts --bins named more than one executable \`$name\`: $(echo "$found" | tr '\n' ' ')"
  [ -x "$found" ] || die "the build named $found for \`$name\`, which is no executable file"
  cp "$found" "$fresh/$name"
done
echo "$key" > "$fresh/KEY"
rm -rf "$dir"
mv "$fresh" "$dir"
rm -f "$said"
echo "build.sh: $(ls "$dir" | grep -vx KEY | wc -l | tr -d ' ') scripts in $dir under $key"
