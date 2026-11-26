# The emitter's LLVM boundary

This is LLVM option 1 in `docs/v2/plan/third-party.md`: an owned, zero-dependency
Rust crate declaring only the C APIs used by the release emitter. LLVM remains the
pinned native library. This crate does not wrap or copy llvm-sys's Rust bindings.

`functions.txt` selects declarations from `abi.h`. `generate.py` asks Clang to parse
the pinned headers, then emits their opaque types, enums and function signatures:

```
python3 crates/blaze-llvm-abi/generate.py '{"prefix":"/absolute/pinned/llvm","check":false}'
```

Use `check: true` to verify regeneration. The generated C++ assertions are compiled
against the installed headers on every build. Unsupported C shapes stop generation.
The three blaze pass-manager declarations in `abi.h` are checked by including them
before the existing `vendor/llvm-sys/wrappers/passes.cpp` implementation; the wrapper
stays in one place. No other llvm-sys code is linked.

The build shares the existing `vendor/llvm-sys/pin.rs` verifier: only
`LLVM_SYS_231_PREFIX`, with the exact version and commit in `pins/llvm.json`, is
accepted. Link manifests are read directly; older native prefixes answer through
their verified `llvm-config`. The returned static archive list includes LLVM's
support library. C++ is libc++ on macOS; Linux asks the same C++ compiler used for
the wrapper for its static libstdc++. No LLVM search paths are guessed.

Native builds use `c++` and `ar`. Explicit target tools use `CXX_<target>` and
`AR_<target>` (hyphens or underscores), then `HOST_`/`TARGET_`, then plain `CXX`/`AR`.
Cross builds must name both tools. A tool variable is an executable path, not a
shell command. Headers and target archives must be from that target's pinned prefix.

Migration changes only the emitter's imports and dependency. Its IR generation,
diagnostics, identity and object reader are untouched. Validation is regeneration,
the dependency ratchet, a narrow compile, and the emitter's real LLVM release test.
