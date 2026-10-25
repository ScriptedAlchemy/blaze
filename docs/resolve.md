# Reading workspaces as cargo does

blaze reads `Cargo.toml` and `.cargo/config.toml` itself, the way cargo 1.99.0 reads them,
and never runs cargo to do it. `crates/blaze-manifest` is that reader; cargo is only the oracle
`harness/resolve.py` compares it with. Every manifest cargo accepts reads the same, and one
cargo rejects is rejected with cargo's message. Part of the self-contained blaze
([#298](https://github.com/indexable-inc/blaze/issues/298)).

## What it reads

- A workspace as `cargo metadata --no-deps --format-version 1` prints it: the root found from
  the current directory or `--manifest-path` (the roots read so far first, then up the
  directories, stopping at `CARGO_HOME`), members from `members` globs, `exclude`,
  `default-members` and path dependencies under the root, `workspace = true` fields and
  dependencies inherited (features joined, `default-features` merged as each edition does),
  `[workspace.metadata]` and `package.metadata` in their order, targets discovered and
  configured (`[lib]`, `[[bin]]`, examples, tests, benches, build scripts, `auto*`, editions,
  the 2015 inference warning), `publish`, `readme` found by default, `license-file`,
  `rust-version`, `links`, `default-run`, and dependencies renamed, per target, for build and
  for dev, with git and registry sources. `cargo run -p blaze-manifest --example metadata --
  --no-deps` prints the same JSON, warnings and errors.
- `cargo-features`: the stable ones accepted, the unstable ones refused with cargo's message on
  the stable channel, the removed ones refused.
- `.cargo/config.toml` files from the current directory up and in `CARGO_HOME`, merged as cargo
  merges them, with `include`, `--config` (files and dotted keys) and `CARGO_*` variables:
  enough for `build.target-dir`, `build.build-dir` and the crates.io and alternate registry
  indexes so far. As in cargo, the files are read at the first value something asks for, so a
  broken one is an error only where configuration is read.
- A manifest that does not parse or fit the schema is shown as cargo shows it, a snippet with
  the place marked (`blaze_manifest::diagnostic`).

`cargo blaze` reads the workspace with it (`crates/ra-jit/src/cargo/metadata.rs`): every
command's `cargo metadata --no-deps`, with `--manifest-path` and `--config` as the command was
given them. A manifest cargo refuses ends the command with cargo's message and status 101. The
daemon's loads still go through rust-analyzer's project model and cargo's full `cargo metadata`.

Not yet: dependencies resolved (the `resolve` of `cargo metadata`, with the lock file, waits
for `crates/blaze-resolve`), the rest of the config keys (`build.target`, rustflags, `[env]`,
linkers and runners, `[source]` replacement, `[net]`), profiles, and the warnings cargo prints
for unused manifest keys and lints. `{workspace-path-hash}` in `build.build-dir` is refused
until cargo's hash is ported.

## Parity

`python3 harness/resolve.py` on dev-aws-8, 2026-10-25, at commit 00a37aa505 of this branch:
cargo 1.99.0 and blaze-manifest on every workspace of `harness/resolve/corpus.json` and the
projects of `harness/bench.py`, each from its root with a `CARGO_HOME` of the harness's own, the
JSON compared as text with the root and `CARGO_HOME` replaced, key order included.

| Part | Equal | Total |
|---|---|---|
| bench projects of `bench.py` at their pinned revisions (zed, bevy, nushell, helix, …) | 13 | 13 |
| crates.io crates, the 300 most downloaded, newest stable versions | 300 | 300 |
| workspace repositories at pinned commits (rust, servo, polkadot-sdk, agave, deno, wasmtime, uv, ruff, polars, …) | 72 | 72 |

Together 4,204 packages, 11,152 targets and 61,432 dependencies, none different. On the Mac
the same comparison over 226 local workspaces gave 202 equal and 24 that both refused, all 24
with cargo's error text (unstable `cargo-features` and `cargo-features` lists, templates that
do not parse, members that are missing).

The workspaces of `crates/blaze-manifest/tests/fixtures` are the fast tier: cargo's output for
each is committed, and `cargo test -p blaze-manifest` compares without cargo or the network.

## Decisions

**cargo's schema crate, pinned.** `cargo-util-schemas = "=0.14.3"` is the version cargo 1.99.0
is built with: its published source is the same, file for file, as `crates/cargo-util-schemas`
in cargo's 1.99.0 tree. Using it means a manifest deserializes into exactly the types cargo's
does and fails with exactly cargo's messages (package names, versions, `PackageIdSpec`, git
references, lints), and the lock file and index types are there for blaze-resolve and
blaze-registry. It depends on neither the `cargo` crate nor anything that does: serde (with
`serde-untagged` and `serde-value`), `toml` 1.x (the one blaze already has), `semver`, `url`,
`jiff`, `thiserror`, `unicode-ident`. Owning
copies of the types would mean keeping thousands of lines in step with cargo by hand, and every
drift would be a manifest read differently. The pin moves with the toolchain blaze pins; if
blaze ever needs to change one of its types, it is vendored then.

**cargo's renderer, pinned.** `annotate-snippets = "=0.12.16"`, the version cargo 1.99.0 renders
manifest diagnostics with, so that a manifest error reads character for character as cargo's.
