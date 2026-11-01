# Native closure integration receipt

This lane replaces the platform's point-file proxy with complete native trees and
an explicit input-admission API. It binds file bytes, directory membership,
missing candidates, native names, executable state and symlink referents.
Relocating the SDK root preserves identity. Per-unit native settings use the
separate shared wire codec and compiler-instance IDs.

Validation ran on ubuntu-main, Linux x86_64, Rust/Cargo 1.99.0, with independent
4 CPU / 16 GiB caps and private targets. The pinned Nix bench shell used LLVM
20.1.8. No heavy Mac builds ran.

- Native tree tests: 17 passed.
- Closure/admission data tests: 8 passed.
- Environment tests: 7 passed, including known Nix wrapper controls.
- Pinned Nix full platform suite: 17 passed, including the actual System probe
  and repeated full-platform equality (145.67 seconds).
- Link resolver tests: 10 passed, including an actual C compiler/linker comparison
  selecting two different native libraries, raw response-file paths, forwarded
  response files, scripts, thin archives and mutation rejection (0.14 seconds
  after 16.24 seconds compilation).

The ordinary Ubuntu System probe is now a passing capability gate. The complete compiler
search tree reaches `/etc/ssl/private` through `/usr/lib/ssl/private`, a directory
symlink, and an unprivileged `0700` listing of it is denied. That denial is an explicit
observation in the captured tree, not an error, an omission or an empty directory: the
tree binds the name, the directory type and every symlink reaching it, records its
members as unknown rather than absent, and contributes a stable `unreadable` entry to
the tree digest. Admission admits every read outside it — a toolchain that never
descends into it is admitted — and refuses a later read of anything below it by name as
an unbound native input. The other 16 platform cases pass in that run. The whole
31-case platform suite, the System probe included, also passes as an unprivileged user
in a Debian bookworm container with the same `/usr/lib/ssl/private -> /etc/ssl/private`
`0700` layout. Pinned Nix is a separate passing environment. The tree digest and
native-bindings CBOR versions stay `1`: a tree that was capturable before hashes byte
for byte as it did, the new kind tag is disjoint, and a tree that contained the denial
had no saved identity to preserve.

A capture/admission pair covers the shape directly: a `0000` directory reached through
a symlink is captured, digested twice alike, re-rooted from an overlapping capture
alike, round-tripped through the bindings codec, admitted outside, refused by name
below, and seen to become stale once its members can be read. It skips under a process
that may read the directory (root) with a message saying so.

This receipt does not certify complete C2 saved execution. The combined producer
adapter, authenticated native context installation, full original local/macOS
arm, actual saved C dependency and compiler-byte differentials remain required.
Source/producer closure admission must be supplied by that integration; a library
basename alone grants no admission. The old blanket native refusal is replaced
only where the complete explicit binding context is installed.

Evidence remains in the task-owned remote lane
`/home/zack/blaze-validation-20261029-efc485a2/pr18-native/evidence/`:
`closure-tests.log`, `nix-native-system.log`, `native-link-resolver-real.log`,
and `closure-inputs.json`. The 19 transferred implementation/test hashes in the
initial manifest matched after the native and Nix runs; the additional actual-C
resolver test was transferred and rebuilt separately afterward.
