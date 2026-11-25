#!/usr/bin/env bash
set -euo pipefail

mkdir -p "$NAME"
cp LICENSE-APACHE LICENSE-MIT README.md "$NAME/"
for binary in blaze blaze-launcher blaze-fact-shim; do
  install -m 755 "target/release/$binary" "$NAME/$binary"
done
"$NAME/blaze" --version
"$NAME/blaze" toolchain
command -v xz >/dev/null || brew install xz
tar cf - "$NAME" | xz -9e -T0 > "$NAME.tar.xz"
tar czf "$NAME.tar.gz" "$NAME"
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "$NAME.tar.xz" > "$NAME.tar.xz.sha256"
  sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256"
else
  shasum -a 256 "$NAME.tar.xz" > "$NAME.tar.xz.sha256"
  shasum -a 256 "$NAME.tar.gz" > "$NAME.tar.gz.sha256"
fi
ls -lh "$NAME.tar.xz" "$NAME.tar.gz"
