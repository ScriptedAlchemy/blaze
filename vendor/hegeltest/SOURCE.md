# hegeltest, hegeltest-macros, hegeltest-c

Hegel, the property-based testing library blaze's property tests and deterministic simulation
tests use (the `testing` skill, .agents/skills/testing/SKILL.md): Hypothesis's engine ported to Rust, with a Rust frontend.

- Upstream: our fork https://github.com/indexable-inc/hegel (of hegeldev/hegel), commit
  `cae7cfa19251314351738a31234a3b576f2c5421` ("Merge pull request #3 from
  indexable-inc/wasi-backend"), the same commit the ix monorepo pins as its `vendored/hegel`
  submodule. Engine changes go to the fork's `main` by pull request, then come here as a new pin.
- Crates, each from a directory of that commit:
  - `vendor/hegeltest`: `frontends/rust`, crates.io `hegeltest` 0.50.3, lib name `hegel`.
  - `vendor/hegeltest-macros`: `frontends/rust/macros`, `hegeltest-macros` 0.50.3.
  - `vendor/hegeltest-c`: `libhegel`, `hegeltest-c` 0.48.0, lib name `hegel_c`: the native engine.
- Licence: MIT, copyright Antithesis, LLC (LICENSE in each directory, the repository's LICENSE).
- Used only as a dev-dependency, with `default-features = false, features = ["static-engine"]`:
  the engine is linked as a Rust crate, with no Python server and no shared library.
- The three directories are excluded from blaze's workspace (root `Cargo.toml`). They build as
  dependencies of blaze's tests. Changes to the vendored engine or client also run their
  retained embedded tests directly with `blaze test --manifest-path` (the client requires
  `--features static-engine`); their test dependencies stay in their own lockfiles.

Kept from `tests/`: every file under `tests/embedded/` (subdirectories too) that a
`#[cfg(test)] #[path = ...]` module names, and the data files those include with `include!`
(`native/statistics_oracle_data.rs`, `native/unicodedata_python_reference_data.rs`,
`native/unicodedata_python_reference_deltas.rs`), unchanged from the commit above, because
`blaze fmt --all` (fmt.yaml) resolves those modules and fails on a missing file. Until
2026-11-04 only the top-level ones were kept, and the 91 under `ffi/`, `generators/` and
`native/` were missing. Left out: the rest of every `tests/` directory (the integration tests and
libhegel's other embedded tests), `examples/`, `benches/`,
`scripts/`, `include/` (the C header), the justfiles, AGENTS.md, CHANGELOG.md, RELEASE.md, the
`[[bin]]` fixtures and the dev-dependencies those fixtures need, and the hegel workspace's own
`Cargo.toml` and `Cargo.lock`.

Changes, each marked `blaze:`:

1. `hegeltest` has no build script. Upstream's `build.rs` builds the engine as a cdylib with a
   nested cargo when `static-engine` is off and bakes its directory and version into the runtime
   loader; with `static-engine` it only sets those two variables. The loader
   (`src/ffi/sys/loader.rs`) is left out with it, and `src/ffi/sys.rs` stops with a
   `compile_error!` when `static-engine` is off.
2. `hegeltest-c` builds an rlib only (upstream: `cdylib`, `staticlib`, `rlib`).
3. `hegeltest-c` asks for `toml` 1.1.2 (upstream 1.1.6), the version blaze's lock has; the engine
   uses only `toml::de::{DeTable, DeValue, DeString}` and `toml::Spanned`, which 1.1.2 has.
4. The manifests name `[lints.clippy] too_many_arguments = "allow"` (the hegel workspace's
   lints) in place of `[lints] workspace = true`, and turn off target auto-discovery.
5. `hegeltest-c` asks for `miniz_oxide` 0.9.1 (upstream 0.8), the one `flate2` uses in blaze's
   lock; `compress_to_vec_zlib` and `decompress_to_vec_zlib_with_limit` are the same in 0.9.
   Blobs it compressed before still decode (zlib), though the bytes it writes may differ.
6. The `Generator` impl of `BoxedGenerator` and the `PrintableGenerator` impl of
   `BoxedPrintableGenerator` name the outer lifetime explicitly. Stage-0 blaze 0.20261030.1
   rejects their `boxed` methods' `Self: 'b` bound when that lifetime is anonymous (#1678),
   blocking the test dependencies. This keeps the same lifetime-generic impls and gives the
   bootstrap compiler names it can check; the general compiler bug remains open.
7. Generator scopes measure the union of nested and concurrent generation intervals for
   the TooSlow health check, including custom client generators. Property-body time stays
   in execution statistics. The vendored static client uses an internal Rust function to
   open and close scopes; the public C ABI and replay data are unchanged. The retained
   embedded tests have their `tempfile` and engine `serde_json` dev-dependencies restored.
