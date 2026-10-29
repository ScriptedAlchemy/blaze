# Owned cold lock resolution

The application resolver now creates a missing `Cargo.lock` and resolves a stale
one itself. It uses the existing resolver and source adapters for registry and
Git candidates, transitives, previous-lock preferences, backtracking and MSRV
ordering. Malformed lock files and read errors remain errors.

All five product metadata callers carry `locked` and `offline` policy. Direct
compiler arguments and Cargo command selection preserve those flags through
worker serialization; `--frozen` enables both. The installed sysroot lock is
always locked, with offline policy captured from the application selection.
Owned workspace-root or metadata errors propagate without retrying Cargo.

Independent review identified four defects in the first implementation. The
integrated change keys Git summary caches by their precise source URL, refreshes
a missing locked sparse-index candidate online, retains explicit crates.io
identities in alternate-registry dependencies, and publishes complete lock
contents atomically. Publication preserves file permissions and symlink targets,
refuses read-only authoritative files, and removes temporary staging on error.
The returned source evidence contains the resulting lock bytes.

Validation on `ubuntu-main`, Linux x86_64, Rust/Cargo 1.99.0:

- `cargo test --locked -p blaze-metadata -p blaze-registry -p blaze-resolve --all-targets -- --test-threads=2`: 191 passed.
- Metadata example against independent Cargo workspaces: 33 comparisons passed,
  including advancing Git patches, incoming locks newer than a sparse cache,
  cross-registry dependencies, and read-only locks. Product Cargo/rustc/rustdoc
  blockers recorded zero calls.
- Two direct/Cargo CLI lock-policy tests passed; native debug product binaries
  built successfully.
- The strict product CLI initially exposed the loader's Cargo retry on an owned
  `--locked` error. With that retry removed, missing-lock `--locked` and `--frozen`
  return errors with empty proxy and absolute-exec audits. Positive cold compile
  acceptance is recorded separately from these negative cases.
- Daemon Nix graphs regenerated for all four targets with pinned Cargo 1.99;
  only the lock hash changed. This is graph generation, not a Nix product build.
- Formatting, harness imports, compiler-site audit and focused actionlint pass.

CI runs the module tests and the independent Cargo comparisons. Existing
`[replace]`, authenticated-registry and Git-index registry gaps remain visible
errors in owned mode. This receipt does not claim final combined-head producer,
bootstrap, cross-machine, macOS, or LLVM acceptance.

Remote evidence is under the task-owned
`sourcecheck/evidence/cold-final-{units,example,oracle}.log`,
`cold-review-flags.log`, `cold-strict-build.log`, `cold-nix-graph.log`, and
`cold-cli-fallback-before/`. The strict product trace is in
`sourcecheck/cold-cli-strict/` until final evidence collection and cleanup.
