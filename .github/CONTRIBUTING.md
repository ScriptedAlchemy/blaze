# Contributing

The long version, with the rules for code and how to change the vendored rust-analyzer, is
[docs/contributing.md](../docs/contributing.md). The short version:

## Build

```
git clone https://github.com/indexable-inc/blaze
cd blaze
sh site/install.sh --version "$(cat .github/blaze-version)"
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
blaze build --locked -p blaze-engine --release
```

## Test

```
blaze fmt --all --check
blaze test --release --locked -p blaze-engine -p hir-ty -p hir-def -p parser -p syntax
```

`python3 harness/unittests.py` runs CI's Blaze unit-test selection, over blaze's crates and the
front end's (`hir-ty`, `hir-def`, `parser`, `syntax` and the rest), all in `crates/`.
The platform needs a Blaze bootstrap and its native C/C++ tools. A bootstrap failure is work
to fix; the unit-test helper has no Cargo mode.

The real gate is real crates' own test suites passing under blaze, and tests that state their
expected result: see [docs/harness.md](../docs/harness.md). Run what fits your change and say in the pull request
what you ran and on which platform.

## Changes

- Add a reduced case that fails before your change and passes after.
- Add a line under `## [Unreleased]` in [docs/CHANGELOG.md](../docs/CHANGELOG.md) for anything a user
  would notice.
- Credit anyone whose idea, diagnosis or patch the change carries with a `Co-authored-by:`
  trailer: [docs/contributing.md](../docs/contributing.md#credit).
- Contributions are accepted under MIT OR Apache-2.0, the project's licence.

Security problems go through [SECURITY.md](SECURITY.md), not public issues.
