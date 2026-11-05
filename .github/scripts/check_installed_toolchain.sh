#!/bin/sh
# Both native candidates and published releases must name a real installed
# toolchain. These two CLIs intentionally print different identity formats.
set -eu
info=$(blaze toolchain)
printf '%s\n' "$info" >&2
case "$info" in
  'blaze builds with Rust 1.99.0 from rustup, at '*)
    directory=${info##*, at }
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
