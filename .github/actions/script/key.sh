#!/usr/bin/env bash
# The built scripts' source key, from a complete checkout, with no compiler invocation.
# lock.awk validates Cargo's generated lockfile subset and the complete scripts closure;
# packages.mjs uses Bun's TOML parser to resolve local identities through their declared
# paths. Unsupported or missing inputs are errors, never successful partial keys.
#
# Hash every local package's files (including vendor packages), the relevant lock blocks,
# Cargo.toml and blaze.lock whole, the compiler/toolchain pins, .cargo/config.toml, this
# action, the host and the absolute checkout path (baked-in CARGO_MANIFEST_DIR). Hashing the
# whole root manifest deliberately includes transitive workspace and patch settings.
# Only .git and target directories are excluded from source trees. Symlinks and special
# files are refused. This repairs source/manifest/lock coverage; it makes no new claim about
# native compiler or ambient environment inputs beyond build.sh's existing policy.
set -euo pipefail
export LC_ALL=C

sha() { if command -v sha256sum >/dev/null; then sha256sum; else shasum -a 256; fi; }
die() { echo "::error::$*" >&2; echo "key.sh: $*" >&2; exit 1; }
root=$(pwd -P)
here=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
top=$(git rev-parse --show-toplevel) || die "no checkout at $root"
[ "$top" = "$root" ] || die "run from the checkout's top, not $root"
if sparse=$(git config --bool core.sparseCheckout); then
  [ "$sparse" = false ] || die "sparse checkout: materialize the whole checkout before computing the key"
else
  status=$?
  [ "$status" = 1 ] || die "cannot read sparse checkout configuration"
fi
for need in .github/blaze-version harness/toolchain Cargo.toml Cargo.lock blaze.lock .cargo/config.toml scripts/Cargo.toml; do
  [ -f "$need" ] && [ ! -L "$need" ] || die "missing or symlinked input $need"
done
command -v bun >/dev/null || die "Bun 1.4.5 is required to parse Cargo manifests"
bun_version=$(bun --version) || die "cannot read Bun version"
[ "$bun_version" = 1.4.5 ] || die "Bun 1.4.5 is required, found $bun_version"

work=$(mktemp -d "${TMPDIR:-/tmp}/blaze-script-key.XXXXXX")
trap 'rm -rf "$work"' EXIT
: > "$work/locals"
awk -v locals="$work/locals" -f "$here/lock.awk" Cargo.lock > "$work/closure"

# Actual dependency paths bind identities to sources; a same-name directory elsewhere
# cannot substitute for a missing declared package. The helper also records ancestors
# that supply workspace inputs, whether tracked by git or ignored.
bun "$here/packages.mjs" "$work/locals" "$work/used-manifests" > "$work/directories"
printf '%s\n' .github/actions/script >> "$work/directories"
sort -u "$work/directories" > "$work/sorted-directories"
: > "$work/files"
cat "$work/used-manifests" >> "$work/files"
while IFS= read -r dir; do
  physical=$(CDPATH= cd -- "$dir" && pwd -P) || die "missing local source directory $dir"
  expected=$root
  [ "$dir" = . ] || expected="$root/$dir"
  [ "$physical" = "$expected" ] || die "symlinked or unsupported local source directory $dir"
  find "$dir" \( -type d \( -name .git -o -name target \) -prune \) -o -print0 > "$work/tree"
  while IFS= read -r -d '' file; do
    case "$file" in *$'\n'*|*$'\t'*) die "unsupported source path: $file" ;; esac
    [ ! -L "$file" ] || die "symlinked source input $file"
    [ -f "$file" ] || [ -d "$file" ] || die "unsupported source input $file"
    printf '%s\n' "$file" >> "$work/files"
  done < "$work/tree"
done < "$work/sorted-directories"
printf '%s\n' .github/blaze-version harness/toolchain Cargo.toml blaze.lock .cargo/config.toml >> "$work/files"
sort -u "$work/files" > "$work/sorted-files"
{
  printf 'host %s %s\n' "$(uname -s)" "$(uname -m)"
  printf 'checkout %s\n' "$root"
  cat "$work/closure"
} > "$work/inputs"
while IFS= read -r file; do
  if [ -d "$file" ]; then
    printf 'directory %s\n' "$file" >> "$work/inputs"
  else
    sum=$(sha < "$file") || die "cannot hash source input $file"
    sum=${sum%% *}
    [ "${#sum}" = 64 ] || die "invalid SHA-256 for $file"
    case "$sum" in *[!0-9a-f]*) die "invalid SHA-256 for $file" ;; esac
    printf 'file %s %s\n' "$file" "$sum" >> "$work/inputs"
  fi
done < "$work/sorted-files"
digest=$(sha < "$work/inputs") || die "cannot hash the complete inputs"
echo "scripts-$(uname -s)-$(uname -m)-$(cat .github/blaze-version)-${digest%% *}"
