---
status: record
summary: The fork's lint definitions and workspace-level parser before the remaining V1 crates were removed; current lint work lives in blaze-lint and blaze-lint-defs.
date: 2026-11-21
---
# Lint definitions and the replacement runner

This is the pre-integration account. The later upstream deletion removed `hir-lint-levels`
and the old workspace parser described here. Current implementations are in
`crates/blaze-lint` and `crates/blaze-lint-defs`; this record does not describe their
present feature coverage.

The old `blaze-lints` runner, its passes and baselines, and the compiler daemon were deleted
in the upstream rewrite on 2026-11-15. The CLI and the front-end level resolver remain
[parked](v2/plan/parked.md). This checkout does not yet provide the old `blaze check` lint
pipeline or its `--fix` and `--baselines` commands. References to those commands in retained
source comments describe the previous integration.

`crates/blaze-lint-defs` remains: it defines lint names, groups, default levels, command-line
options and readiness switches. It contains both the pinned Rust lint catalog and the
`blaze::` definitions, with tests of the catalog in `crates/blaze-lint-defs/src/tests.rs`.
A registered definition says what a lint means; it is not an executable lint pass.
The parked `crates/hir-lint-levels` resolves scoped levels, and the parked
`crates/blaze-units` retains the workspace configuration parser described below.

The replacement design makes each finding an item-relative record, applies levels when
presenting the result, and schedules errors before advice. Its scope and status are in
[bottom.md](v2/plan/design/bottom.md) and
[target.md](v2/plan/design/target.md). This page documents the surviving definitions and
interfaces; it does not assert that the replacement has restored the old checks.

## Names, levels and readiness

The authoritative catalog is `crates/blaze-lint-defs/src/lints.rs`; group membership and
renamed or removed names are in `crates/blaze-lint-defs/src/names.rs`. The groups include
`blaze::general` for broadly useful checks, `blaze::internal` for repository conventions,
`blaze::pedantic_perf` for optional performance advice, and `blaze::edition` for edition
adoption. The catalog's per-lint level is authoritative: membership in a group does not
mean every member has the same default.

`Level` represents `allow`, `expect`, `warn`, `force-warn`, `deny` and `forbid`. Command-line
parsing and attribute resolution are separate from running the check. The readiness
constants in `crates/blaze-lint-defs/src/ready.rs` keep advice from claiming a replacement
is ready merely because its lint name exists:

| Definition | Catalog default | Readiness condition |
|---|---|---|
| `blaze::proc_macro_derive` | `allow` | `REFLECTION_REPLACES_MACROS`, currently `false`; warns when the reflection replacement is ready. |
| `blaze::proc_macro_attribute` | `allow` | The same reflection switch. |
| `blaze::proc_macro_fn_like` | `warn` | No readiness switch; its proposed replacement is a Rust `const fn` or `const` block where applicable. |
| `blaze::build_script_ambient` | `allow` | `CAPABILITIES_FOR_BUILD_SCRIPTS`, currently `false`; waits for a capability-based entry point. |
| `blaze::not_latest_edition` | `allow` | `EDITION_SHIPS`, currently `false`; waits for the edition and its library to ship together. |
| `blaze::std_use` | `allow` | Its own default stays `allow`; `BSTD_IN_TREE`, currently `false`, controls the workspace's `lints-when-ready` entry. |

These are definition defaults, not evidence that any of these checks ran on this tree.

## Workspace levels and the ratchet

The retained parser is `crates/blaze-units/src/load/plan/flags.rs`. It reads Blaze-specific
levels from manifest metadata, which Cargo does not pass to rustc as an unknown lint tool:

```toml
[workspace.metadata.blaze.lints]
general = "warn"
hash_twice = "deny"
```

Names omit the `blaze::` prefix. Allowed table values are `allow`, `warn`, `deny` and
`forbid`; an unknown name or level is an error. Wide groups precede narrower groups, and
single-lint settings follow them, so the more specific level wins.

The parser also retains two related tables:

- `lints-when-ready` sets a lint's level only when its named readiness switch is true. A
  lint without such a switch is refused in that table.
- `lints-ratchet`, when present, supplies levels in place of `lints`, after readiness
  settings. Its `baselines` field names a directory relative to the workspace root.
  Each `blaze.<lint>.txt` lists `crate<TAB>count` or `crate<TAB>unmeasured`; a listed crate
  receives a later warning-level flag for that lint. A missing directory, malformed line
  or unknown lint is refused. `blaze::unreachable` cannot be lowered below `deny`, including
  by a baseline.

The old repository policy used these files to keep existing findings at `warn` while new
crates started at `deny`, with baseline membership and counts permitted only to shrink.
The root manifest no longer contains that ratchet table, and the old runner's baseline
directory was removed. The surviving parser does not by itself reinstate the old gate.
Current standalone source checks and their commands are documented in
[contributing.md](contributing.md); they are distinct from executing the deleted lint
passes. Reconnecting a lint runner must include actual finding and baseline validation,
not just recognition of these tables.

## Rules referenced by surviving code

The following definitions explain references in the libraries and harnesses that remain.
Their default levels come from the catalog. Each replacement below is a coding rule or
retained API, not a claim that an automatic fix is currently available.

### `as_cast`

Catalog default: `allow`. An `as` cast hides whether it widens, truncates, changes sign,
changes float rounding or manipulates pointer provenance. The rule names that operation
explicitly. `crates/blaze-cast` retains the named conversion APIs used by the previous
fixes; ordinary infallible conversions can use `From`, and pointer operations use the
corresponding pointer methods.

### `tmp_path`

Catalog default: `allow`. The definition flags manually assembled paths under the system's
temporary directories and direct `std::env::temp_dir` use outside the shared helper.
`crates/blaze-core/src/temp.rs` provides `TempDir`, which owns its directory and removes
what it created when dropped. Use that owner rather than a fixed path shared by parallel
tests.

### `absolute_position`

Catalog default: `allow`. A hashed or persisted value must not depend on an absolute file
position that moves when unrelated lines are inserted above it. The definition covers
source locations and ranges stored in such types. The item-relative position types in
`crates/blaze-core/src/site.rs` preserve the intended distinction; file, line and column
belong in the rendered diagnostic.

### `unbounded_test_wait`

Catalog default: `allow`. A test's park, condition wait, channel receive or child-process
wait must have a deadline; repeatedly polling `try_wait` without checking a deadline is
still unbounded. `crates/stdx/src/child.rs` retains the child-process wait helper. A timeout
must fail with evidence of what was waiting, rather than leave a test indefinitely blocked.

### `std_use`

Catalog default: `allow`. The definition identifies standard-library operations that bstd
replaces or forbids, including ambient authority, hidden allocation and renamed APIs.
The migration design is [std.md](v2/plan/design/std.md). The readiness switch remains false;
`harness/v2/std_uses.py` retains a census command for the old lint runner, so that script is
not evidence of a working current-tree compiler or a completed migration.

### `not_latest_edition`

Catalog default: `allow` while `EDITION_SHIPS` is false. The rule describes one finding per
workspace package that has not adopted the latest Blaze edition. The retained regression
in `crates/blaze-engine/tests/repro.rs` belongs to the parked compiler. The
[edition design](v2/plan/design/edition.md) is the home for its intended adoption behavior.

### `workspace_unsafe`

Catalog default: `allow`. The definition asks a workspace crate that contains unsafe code
to state its reason at the root with `#![blaze::trust(unsafe, reason = "...")]`.
`harness/toolchain_bump.py` retains the related source-root handling. This declaration is
separate from the dependency capability policy in the
[trust design](v2/plan/design/trust.md), and it does not replace checking the unsafe code.

### `borrow_workaround`

Catalog default: `warn`. The definition identifies a value looked up once only to test
whether it exists, then looked up again for use, when one retained lookup in `if let`
expresses the operation. The intended analysis and its boundaries are in
[lint-borrow-workarounds.md](v2/plan/design/lint-borrow-workarounds.md).

### `checkfree`

Catalog default: `deny`. This is the contract for a marked `#[blaze::checkfree]` function:
its optimized code must contain no remaining bounds check, overflow check or panic.
The default applies to the catalog entry; enforcing the contract still requires a runner
and optimized-code analysis. The related design is
[vecreport.md](v2/plan/design/vecreport.md).

### `vecreport`

Catalog default: `allow`. The definition describes reports of checks and panic paths
remaining in optimized loops, and the reasons loops were not vectorized. It is
performance advice from optimized code, with profiling restricting attention to hot
loops when available. The [design](v2/plan/design/vecreport.md) describes its intended
release-build integration; the deleted runner's reports are not a current capability.

## Suspicious code

The catalog retains definitions such as `skipped_pair` (an early exit skips the second
operation), `half_done` (fallible exits leave partial mutation), `hidden_exit` (a macro
hides control flow), `fallback` (an error is discarded in favor of another result), and
`placeholder_body` (a promised effect is replaced by constant success). These internal
definitions are `allow` by default until explicitly selected by a policy.

The root `Cargo.toml` separately configures Clippy's `allow_attributes` and
`allow_attributes_without_reason`. They address unnecessary `allow` attributes and
unexplained suppression without a duplicate Blaze lint. The former automatic conversion
to `expect` depended on the deleted runner. A retained definition or manifest comment
does not establish that an expectation was checked.

## Duplicates

The retained definitions distinguish identical syntax (`same_fn`), near-duplicate code
(`near_duplicate`), parameterizable duplication (`extractable_duplicate`), copies across
crates (`duplicate_across_crates`), and equivalent behavior (`same_behaviour`). They are
internal, `allow` by default, and need their analyses rebuilt before findings can be made.
Prefer one implementation of a shared rule; equivalence is not established merely by
similar spelling.

Clippy's expression-level checks remain configured in the root manifest: `eq_op`,
`ifs_same_cond` and `self_assignment` are covered by `correctness`, and
`if_same_then_else` is explicitly denied. These manifest settings are distinct from
the deleted Blaze duplicate-analysis passes.

## Complexity

The catalog records separate measures rather than one generic size score:
`function_weight` for cognitive complexity, `path_count` for acyclic paths,
`live_state` for mutable places across a call, `effect_breadth` for kinds of effect,
`exits` for early exits with cleanup obligations, and size measures such as
`long_function`, `long_file`, `many_params` and `wide_type`. All are `allow` by default.
Their former thresholds and per-item baselines belonged to the deleted analysis layer;
the catalog alone neither computes a measure nor proves that a function is below a bound.
