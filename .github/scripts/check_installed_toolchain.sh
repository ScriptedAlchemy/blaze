#!/bin/sh
# Native candidates can select an explicit installation, rustup, or their own
# copy. Published releases can use the older toolchain identity format.
set -eu
info=$(blaze toolchain)
printf '%s\n' "$info" >&2
case "$info" in
  'blaze builds with Rust 1.99.0 from rustup, at '* | \
  'blaze builds with Rust 1.99.0, the toolchain at '* | \
  "blaze builds with Rust 1.99.0, blaze's own copy at "*)
    directory=${info##* at }
    ;;
  *"blaze's toolchain ("*') at '*)
    directory=${info##* at }
    ;;
  *)
    printf '%s\n' 'unknown Blaze toolchain identity' >&2
    exit 1
    ;;
esac
case "$directory" in
  /*) ;;
  *) printf '%s\n' 'Blaze toolchain path is not absolute' >&2; exit 1 ;;
esac
if [ ! -d "$directory" ]; then
  printf '%s\n' "Blaze toolchain directory does not exist: $directory" >&2
  exit 1
fi
if [ ! -d "$directory/lib/rustlib/src/rust/library" ]; then
  printf '%s\n' "Blaze toolchain library sources do not exist: $directory" >&2
  exit 1
fi
