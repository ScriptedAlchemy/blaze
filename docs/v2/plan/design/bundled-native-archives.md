---
status: current
summary: Bundled static native libraries are resolved using their declaring crate's search paths before V2 combines the final link line, so dependencies can carry different archives with the same file name.
date: 2026-11-08
---
# Bundled native archives

Two packages can build different static archives with the same file name. A bundled
library belongs to the crate that declared it: resolving every declaration against the
final executable's combined search paths loses that ownership and can link one archive
twice while omitting the other.

V2 resolves each crate's bundled static libraries against that crate's rustflags and
build-script search paths, in their existing order, followed by the C toolchain's
directories. The existing native-library resolver supplies platform naming and errors.
The selected path becomes a verbatim absolute library name before crate lists are
combined. The final linker still reads and hashes those files and follows their native
dependencies. Whole-archive behavior is preserved. Explicitly unbundled static libraries,
dynamic libraries and frameworks keep the final link's search behavior.

This changes no persisted format. The rejected alternative is globally renaming archives:
build-script output names are package-owned and cannot guarantee uniqueness across all
dependencies. Cost is one search per bundled declaration; existing toolchain answers stay
cached, directory listings are shared within a crate, and no archive bytes are read while
binding names.

Regression coverage uses two package directories with different same-named archives and
reverses the final search order. A control omitting binding reproduces the missing second
archive. Additional controls preserve unbundled search, verbatim and whole-archive inputs,
relative search paths and the ordinary not-found error.
