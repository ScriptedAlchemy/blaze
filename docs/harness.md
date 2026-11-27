---
status: current
summary: The harnesses that test blaze (real crates' own tests, and blaze held to itself), what each checks, and how CI runs them.
---
# The harnesses

blaze is held to real crates' own test suites and to itself, not to rustc. No harness compares
blaze's output with rustc's or cargo's: that was dropped on 2026-10-30 ("we use other ppl['s]
test[s] [of] the other crates ... it over-constrains us", Andrew), so how a rule of rustc's is
proven ported is a crate whose own tests exercise it, or a unit test
([AGENTS.md](../AGENTS.md), best-long-term-design). Benches may still time cargo as their
baseline.

On 2026-11-15 the daemon, `blaze build`, blaze-engine and with them most harness scripts were
deleted; they stay in git history. What is left is the scripts below and the workspace's unit
tests, which CI runs with cargo ([CI](#ci)).

## Running them

The scripts run from the repository's root in its Python workspace (`pyproject.toml`,
`uv.lock`): `uv run --frozen python harness/<script>.py '<json>'`. A script moved to one typed
input (AGENTS.md, scripts-take-typed-input) reads it with the workspace's `typed_input`
(`tools/typed-input`); the fields are in each script's docstring. Those that ask rustc for the
host need the pinned nightly (`pins/toolchain`).

## What each one checks

### Correctness of generated code

| Script | What it checks |
|---|---|
| `answers.py --write` | Not a check: writes what the pinned rustc answers to the questions blaze answers in its place (`-vV`, `--print cfg`, cargo's probe of file names, `target-spec-json`) into `pins/answers/rustc.json`, after a toolchain bump, on each CI host ([no-rustc.md](no-rustc.md), step B) |
| `toolchain_pins.py` | That the sha256 and size of each file of blaze's own Rust (`pins/rust.txt`) are what the release's manifest at static.rust-lang.org says, for every host blaze ships for; `'{"write": true}'` regenerates them when blaze moves to another release ([cargo.md](cargo.md)). Needs the network |
| `nightly_features.py '{"dry_run": true}'` | Not a check: keeps the GitHub issue tree of the pinned nightly's unstable features (#967), so every `#![feature(..)]` blaze has to accept is tracked. Each language feature of rustc_feature's `unstable.rs` at library/UPSTREAM's commit is an issue labelled `nightly-feature` under a group of at most 100 sub-issues; the library features (the `#[unstable]` and `#[rustc_const_unstable]` attributes of the sysroot crates in library/) are one issue, #969, for what blaze has to do for all of them (`'{"library": true}'` makes one each). A rerun creates only what is missing (by a hidden marker in each body), links what is not under its group, and comments on and closes the issue of a feature stabilized or removed since. `'{}'` makes the changes, paced under GitHub's limits. Needs `gh` |

### Reading the workspace as cargo does

The workspaces of `crates/blaze-manifest/tests/fixtures` (inheritance, members and targets
discovered, a package outside its workspace's members, config and manifest errors) have their
expected output committed next to them, so the crate's unit tests check them without the
network. The files were first written from what cargo printed; they are not written from cargo
again, and a change to one is a change to what blaze expects.

### Timing

Nothing here gates; a build's profile answers why a build was slow
([profiling.md](profiling.md)). The bench driver was deleted with `blaze build` on 2026-11-15;
the scripts that store, compare and draw its results are left. `benchhistory.py` appends a run to `docs/benchmarks/history/` (what the
site's /bench page charts) and, with `check`, fails when a median moved past its noise band:
instructions first, with a 1% band, then wall time and memory with theirs, after a summary of
what regressed and improved. `counters.py` is how every harness counts instructions and cycles
(Linux's `perf_event_open`, macOS's `proc_pid_rusage`), null with the reason where it cannot
([benchmarks.md](benchmarks.md#instruction-counts)).
`memory.py` is how every harness measures memory: `memory.run` runs a command and returns its
time with the exact peak of its largest process (`wait4`) and the peak of its whole tree
(sampled), `memory.Sampler` follows a long-lived process with its children,
and `memory.in_child` measures a function that runs commands itself
([benchmarks.md](benchmarks.md#memory-and-disk)); a harness that reports memory uses it rather
than a way of its own. Every published benchmark is a matrix over the platforms of
`benchplatforms.py` (macOS arm64, Linux aarch64, Linux x86-64): it names a result's platform from
its host, files it as `docs/benchmarks/<platform>.json`, and gives the workflows the platforms
whose runner is registered. `benchreport.py` writes each table from the newest stored result of
every platform, so one that did not run keeps its last numbers. `benchcharts.py` draws the
benchmark charts (SVG, a light and a dark version, a bar per platform) from the same results;
`benchreport.py` regenerates them whenever it writes a table, `--charts` on its own, and
`--render` every table and chart. With one typed JSON input (`'{}'` for the repository's own
paths) `benchcharts.py` also draws the trends, blaze against its own past runs, from
`docs/benchmarks/history` into the README and benchmarks.md
([benchmarks.md](benchmarks.md#over-time)).

### Which packages a change touches

`scripts/src/bin/changed.rs` checks nothing itself: it prints the workspace packages a change
touches and the packages that depend on them, each after what it needs, as `-p` arguments
(`"print": "args"`), for a check of a change in minutes
([contributing.md](contributing.md#checking-a-change-fast)).

## Adding a test

A bug found in a real crate is proven fixed by that crate's own tests passing under blaze
(`blaze test` in the crate), or, where it
is small enough to state, by a unit test beside the
code that was wrong. A test states its expected result itself; none is "what rustc does".

## Reproducing a failure

Start from the script's last lines; the stepwise ones take `--only TEXT`, `--from N` and
`--keep` (see each docstring).

For wrong generated code:

1. `blaze codegen-inputs --format=json` lists each instance the last build made with its code
   hash, and `blaze trace --out PATH` writes every result of the last build with its edges, as
   CBOR. (v1's `--report`, `--deny` and `--isolate`, which listed, left to rustc and split out
   functions, are gone with its crate-at-a-time entry.)
2. Reproduce against a unit test in `crates/blaze-body`, `crates/blaze-build`, or `crates/blaze-emit` to inspect the lowered body or LIR.
3. Reduce it to a test ([Adding a test](#adding-a-test)), and fix it there.

For a CI failure, the last line of the harness step lists which scripts failed.

## CI

CI has one required aggregate, `ci`, in `blaze.yaml`. This fork uses GitHub-hosted runners
([runners](#runners)); it does not depend on access to indexable's ix machines. The current
workflow separates native library checks, source-compiler checks and production distributed
acceptance so their results cannot be confused.

| Workflow | Event | What runs |
|---|---|---|
| `blaze.yaml` (`changes`) | pull request, merge-queue entry, push to main | `scripts/src/bin/gate_changes.rs`: whether the change touches a file the `cargo` job reads (below) |
| `blaze.yaml` (`scripts`) | every change | ruff and pyright over the Python workspace, `harness/imports.py`, the selftests of `harness/failures.py` and `harness/v2/ran.py`, `harness/library.py check`, the agent rules (`tools/agents/rules.ts --check`), the Rust scripts of `scripts/src/bin` (the docs, `rustcsites`, the ledger, `changed`, the invariants, the lint table, the hash domains, and the ratchets of `tools/ratchets` and of persisted layouts), and `harness/v2/lean_gate.py`'s checks that need no Lean |
| `blaze.yaml` (`lean`) | every change | the Lean proofs (`harness/v2/lean_gate.py`, [v2/plan/proofs.md](v2/plan/proofs.md)): `lake build` of `lean/` and `lean/analysis` with the pinned Lean, no `sorry`, and each theorem of `lean/theorems.json` held to Lean's three axioms by `#print axioms` |
| `blaze.yaml` (`workflows`) | every change | actionlint over `.github/workflows` (below) |
| `blaze.yaml` (`cargo`) | a change of code | The pinned Cargo checks every workspace target except `blaze-std`, then runs the workspace tests through `ci_test`. |
| `blaze.yaml` (`distributed-core`) | every change | All targets and the tests of `blaze-wire`, `blaze-dist`, `blaze-dist-local` and `blaze-serve`, including named causal controls. This is native library evidence. |
| `blaze.yaml` (`source-compiler`) | a change of code | Builds the current `blaze-cli` binaries and retained services, verifies source and binary identity, and runs real cold, warm, body-edit and invalid-source controls. |
| `blaze.yaml` (`production-acceptance`) | a change of code | Runs the retained two-worker and fault acceptance against those sealed binaries and requires complete dev/release receipts. A missing compiler bridge is a failure. |
| `blaze.yaml` (`ci`) | always | whether every job the tier expected ran and succeeded: the one check the branch rules require |
| `fmt.yaml` | push to main | `blaze fmt --all` with the release `.github/blaze-version` names, and a commit `fmt` when it changed a file |
| `llvm.yaml` | a change of its recipe, by hand | the pinned LLVM (`pins/llvm.json`) built for each target by `.github/scripts/llvm.py` on hosted runners, and put into R2 (`.github/scripts/r2.py`) |
| `site.yaml` | a change of the site, by hand | builds and checks `site/`; a push to main deploys it |

The `cargo` job runs only for a change that touches a file it reads; any other change runs the
rest alone. `scripts/src/bin/gate_changes.rs` (the `changes` job) names those files, and why
each: the crates (`crates/`, with their test data), the vendored crates (`vendor/`), astlog
(`tools/astlog/`), `Cargo.toml`, `Cargo.lock`, `.cargo/`, `.rustfmt.toml` and `.clippy.toml`,
the pinned nightly (`pins/toolchain`), `blaze.yaml` and the actions it uses (`toolchain`,
`rustup`, `script`, `blaze`), the release of blaze that builds the scripts
(`.github/blaze-version`), and `scripts/`; on a push to main, the smoke corpus too
(`harness/smoke.py`, `harness/smoke/`). The fork also includes its acceptance harnesses,
CI helpers and Nix recipes in that classification. A file list that cannot be obtained
counts as code. When a required job begins reading a new input, add it to the classifier.

Every job that runs a script (`changes` and `scripts`) takes the built scripts from
`.github/actions/script`. Its Actions cache key covers the complete local source dependency
closure, manifests, relevant lockfile records, build configuration and pinned toolchain
(`key.sh` defines the inputs). Computing that key requires a full checkout. A verified cache
hit reuses the binaries; a miss installs `pins/toolchain`, builds `-p scripts --bins --locked`
with Cargo and saves the resulting executables. `local.sh CACHE` runs the same build and
cache checks outside Actions, using a directory for the cache and an already installed
pinned Cargo toolchain.

A change reaches main through a **pull request** alone
([contributing.md](contributing.md#landing-a-change)), merged once `ci` passes on it: a change
that does not type-check, breaks a unit test or fails a script's check does not merge.

No check refuses a pull request for its formatting: `fmt.yaml` formats main after each push and
commits the result as `fmt`. Run `blaze fmt --all` before pushing all the same, so that the
diff of a pull request is its change.

`harness/imports.py` has every script of the harness imported, so one that only a person runs
can't break unnoticed: a script does nothing when imported (its work is under `if
__name__ == "__main__":`). `scripts/src/bin/rustcsites.rs` fails when blaze's code gains a place that
starts rustc, rustdoc or cargo ([no-rustc.md](no-rustc.md)).

actionlint (the release binary, pinned by its sha256 in `blaze.yaml`'s `workflows` job) checks
every workflow, and each local action where a workflow uses it, on every pull request: a workflow
whose YAML does not parse fails on push with no jobs and no error anywhere (#1691). Its shellcheck
and pyflakes passes are off; `.github/actionlint.yaml` lists what it ignores, each with why.

The repository's Python is one uv workspace (`pyproject.toml`, `uv.lock`, Python 3.13 from
`.python-version`; shared helpers such as `tools/typed-input` are its members). Its scripts run
with `uv run --frozen`, and the gate runs ruff and pyright over the ones the workspace lists
(`pyproject.toml`'s `include`); an older script moves in when it is next changed. `harness/imports.py` runs in the workspace's Python too.

### Runners

The fork's workflows select GitHub-hosted Linux and macOS images. Native compilation and
execution stay on the workflow's stated architecture; local source review or Python checks
are not substitutes for those results. The pinned nightly comes from `pins/toolchain` and
the toolchain shelf is generated and verified by the canonical `toolchain_cut` script.

The workflow does not select ix runner labels or depend on upstream-owned runner snapshots.
Jobs with publication credentials retain their repository and event restrictions. Pull-request
validation does not publish releases or write the shared cache.

### Concurrency, the check and the ruleset

On a pull request a newer push cancels the gate's run before it. Every push to main runs the gate to the end in a concurrency group of its own: with one group for
main, a merge every few minutes replaced each waiting run with the next, and 7 of 10 runs on
main were cancelled.

`ci` is the gate's job that says whether everything that ran passed, and it is the one check
the branch rules require. A job that is renamed or split does not change it. It is red when a
job failed or was cancelled, and when a job its tier expected did not run and succeed.

The live branch rules of ScriptedAlchemy/blaze are repository settings, not an assertion
made by this document. The upstream repository's ruleset does not establish this fork's
rules. Integration stays in its draft pull request until the required checks and production
acceptance are complete; an absent receipt or skipped required job is not a pass.

What is known to be wrong is written down, says why, and does not fail the run. Once it comes
out right, it fails the run until it is taken off the list:

- `harness/known-failures.toml`: Rust unit tests that fail, by crate and full name, each with
  a one-line cause, held by `harness/failures.py`: the only exemption the unit tests have.

`failures.py` reads a run's log (`blaze test`, libtest's text or JSON format) and fails on a
failing test that is not listed, a listed test that passes, is ignored or no longer exists, a
test binary that did not finish (a crash leaves the rest of its tests unchecked) unless it is
listed as `<binary>::*`, and a package asked for none of whose test binaries ran (its test
build failed) unless it is listed as `*`; each failure names the crate, the test and the tail
of its output. Listed tests the run could not check (their crate did not run, filtered out,
cached by `blaze test`, or `only` for another profile or OS) are counted, so a
partial run never reads as clean. `harness/unittests.py` calls it on its log, with every package of
the workspace asked for; a lane runs it on the crates it touched before saying a failure was
there before:

    uv run --frozen harness/failures.py '{"run": {"packages": ["hir-def", "hir-ty-mir"]}}'
    uv run --frozen harness/failures.py '{"results": ["test.log"], "profile": "dev", "os": "macos"}'

The list is written from a run, never by hand: `python3 harness/unittests.py '{"record": true}'`
runs every test and writes the list again, sorted, each new entry with the first line of its
failure as its cause and `only` the OS it ran on; the entries of other OSes are kept, and an
entry that was there keeps its cause. Run it once on each OS the gate tests on (Linux and
macOS), on the tree to land. `harness/failures.py selftest` (CI's `scripts` job) holds the
checked-in list to the form the recorder writes.

A failure joins the list only with an owner and a cause, in the change that explains it.
