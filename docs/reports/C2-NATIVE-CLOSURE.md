# Native closure integration receipt

This lane replaces the platform's point-file proxy with complete native trees and
an explicit input-admission API. It binds file bytes, directory membership,
missing candidates, native names, executable state and symlink referents; SDK
root relocation preserves identity. Per-unit native settings use the separate
shared wire codec and compiler-instance IDs.

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

The ordinary Ubuntu System probe remains a failed capability gate: the complete
compiler search tree reaches `/etc/ssl/private` through a directory symlink and
unprivileged reading returns PermissionDenied. The failure is propagated; it was
not excluded from acceptance or converted to an empty tree. The other 16 platform
cases passed in that run. Pinned Nix is a separate passing environment.

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
