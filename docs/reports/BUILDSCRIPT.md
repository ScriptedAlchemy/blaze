# Why every edit of tracedecay's CLI went to cargo: its build script printed something else

## Ambient dependency environment follow-up, 2026-10-29

Independent review of `f7b3ca93` found that an ambient `DEP_*` variable without
an active native dependency could still be restored from the previous script
invocation. After changing `DEP_USER_VALUE` from `x` to `new`, the script read
`x` while its fingerprint recorded `new`, so Cargo could retain stale output.

`cargo_sets` now reserves only Cargo's own settings. Ambient `DEP_*` values come
from the current caller, and current native dependency metadata is overlaid
separately. The metadata regression supplies the caller's unrelated prefix
explicitly. A new process regression and the live `newly_watched_env` fixture
cover a recorded ambient dependency variable changing alongside the source.

On `ubuntu-main` Linux x86-64, the native release build passed in 7 m 26 s;
all 50 script tests passed (one measurement ignored). The live Cargo comparison
now prints `3 8` on both sides, with the script's changed output correctly
requiring recompilation. Formatting, harness self-test and whitespace checks
passed. The earlier complete backend suites below preceded this follow-up;
LLVM and the complete harness were not rerun for this environment-only change.


## Review closure, 2026-10-29

The earlier reports and their parking statements describe the revisions they name. The current review inventory
is based on all PR #5 comments and its three review submissions; there are no inline review
threads. The latest findings are in
[the second-check review](https://github.com/ScriptedAlchemy/blaze/pull/5#issuecomment-6488867981).

- **Inactive optional and target-gated `links` edges: fixed.** Declared dependencies are
  candidates. Judgment reads the parent's completed Cargo fingerprint under its lock and
  follows only the native run units present in that fingerprint. Inactive candidates are
  neither locked nor read, even if another workspace member built them. The active dependency
  closure stays locked through the parent execution. `an_inactive_native_dependency_is_not_locked_read_or_run`
  covers an absent inactive unit; `inactive_links` adds two live-Cargo differential fixtures.
- **Inputs changed between judgment and Cargo's leg: fixed.** Each completed execution records
  the old and new watched files and variables, native dependency outputs, current `DEP_*`
  values, the executable and, for a script without watches, the package fingerprint. Before
  consuming it, the stand-in checks those inputs and the execution's output. When they changed,
  it executes the existing real script with Cargo's current environment. No compiler or Cargo
  invocation was added. `continuation_edit` edits the watched program immediately before
  consumption and compares program output and execution counts with Cargo alone. The unit
  regression also covers directories, missing inputs, dropped watches, environment, metadata,
  executable changes, recompilation with identical executable bytes, and changed output.
- **Dropped `DEP_*` metadata: fixed at `7ba43be2`.** The parent receives the dependency's current
  metadata, and a dropped key exposes the current caller's ambient value. The regression
  `a_native_dependency_replaces_its_metadata_in_the_parents_environment` covers both absence
  and an ambient replacement value, plus an unrelated metadata prefix.
- **Unknown `cargo::` key accepted by Cargo: disproved.** The reference is Cargo 1.99.0
  (`5f94df478`), which rejects `cargo::unknown-key=1`. The parser's error is retained; it must
  not become a new handoff to Cargo. `cargo::error` and a failed process also fail this build
  without retry; the empty fingerprint permits a later explicit build to retry.
- **Unlocked current/baseline reads and inherited locks: fixed in the existing head.**
  `Run::of` and judgment hold the Cargo locks for the complete read, and baseline capture
  requires a completed fingerprint. The lock regressions cover busy and absent locks and
  duplicated descriptors. Baseline output and the directory digest are read under one guard.
- **The obsolete `Kept` execution replay: removed in the existing head.** `run_as_cargo_does`
  executes the real script unless this same live request supplies a still-valid completed
  execution. Continuation storage is private, single-use and deleted on return.
- **Pinned LLVM build: fixed.** The first LLVM 20.1.8 build exposed `E0500` in
  `hir-ty-codegen/llvm.rs`: the machine-options borrow overlapped the mutable recorder borrow
  inside the callback that reads the emitted object. Owning the same options across the
  callback fixes compilation without changing the options or lowering rules.
- **Earlier logger, newly watched environment, quiet-package, failed-run, dropped-watch and
  active-links findings:** the existing code has focused regressions and the differential
  rows in `scriptrerun.py`; see the runtime results below.

### Verification of this repair

Heavy work runs on the explicitly authorized `ubuntu-main` host (Linux x86-64) in
`/home/zack/blaze-validation-20261029-efc485a2/pr5`. The coordinator holds the parent claim.
Every heavy command is constrained to eight CPUs, 24 GiB RAM and no swap, with six Cargo
build jobs, `RUST_BACKTRACE=0` and a target directory owned by this lane. The Mac only runs
formatting, static audits and the harness comparison self-test.

The repair's build and harness inputs are committed at
`b1d2d3ac9fb129f4d490701acad848a1cb0c3162`; this report adds no build input.
A length-framed SHA-256 over the 5,156 tracked build, harness, flake and CI files
(44,472,801 bytes) is identical in this worktree and remote staging:
`513d2a548a09a0680727a96cfbc422bec549d8330d8708a3a69d9263bc986218`.

| Check | Result |
|---|---|
| Pinned LLVM 20.1.8 release build, `cargo +1.99.0 build --locked --release -p ra-jit --features llvm --bins` | Pass, 1 m 49 s for the final build |
| `cargo +1.99.0 test --release --locked -p hir-ty -p hir-def -p parser -p syntax -p blaze-build-script -p ra-jit --features ra-jit/llvm -- --test-threads=4` | **1374 passed, 0 failed, 2 ignored**, 16 s once compiled; `ra-jit` alone is 402 passed and one ignored measurement |
| New regressions with only the edge-selection and continuation-input guards removed in remote staging | **47 script tests passed, 2 failed, 1 ignored**; precisely the two new regressions fail |
| Complete `harness/scriptrerun.py`, Cranelift / LLVM | **Both exit 0**, 527 s / 907 s; every program, exit status and execution count matches Cargo |
| Corrected continuation race against real Cargo, Cranelift / LLVM | Both pass: output `3 9`, three executions each; 34 s / 56 s |
| Formatting, `git diff --check`, harness imports, compiler-site audit, comparison self-test | Pass; 138 imports and 15 existing compiler sites, none new |

The release unit retry added the missing test prerequisites inside the temporary container:
pinned Nix Perl, and links to the shell's coreutils at `/bin/sleep` and `/usr/bin/printenv`.
The initial five `ENOENT` failures were prerequisite failures; the same unit command then
passed without changing product code. The shell uses the repository's pinned flake and the
coordinator's official Nix image
`nixos/nix@sha256:f159a58f390677f41e8e900a74b6e2f9c7f27b364122ae6acf7a652ce6c551c0`.
The host's Rustup tree is mounted read-only. The compatibility loader and coreutils links
exist only in that temporary container; no host toolchain or installed Blaze is changed.

The first continuation fixture used an ordinary watched counter. Cargo correctly found
its run fresh and never invoked the stand-in, so the fixture's expected third execution
failed. The corrected fixture drops its watches after judgment, forcing Cargo's local-rule
transition to invoke the stand-in; it edits the watched source at that point and requires
both the new output and the third execution. Existing dropped-watch rows exercise unchanged
inputs without an extra execution.

The complete runtime runs include the bundle edits, counter, logger, quiet package, newly
watched environment, failure and explicit retry, dropped watches, active native metadata,
inactive optional and target dependencies, and the continuation edit. The two inactive
fixtures keep both source edits in the daemon and perform no extra script execution on a
subsequent plain Cargo build. The final container's peak was 6,202,515,456 bytes (5.78 GiB).
The original combined wrapper retains exit 1 from its initial missing-prerequisite unit
run; the unit retry above and both final runtime commands independently exit 0.

Not run: the complete `ci.py` fast/full matrices, full tracedecay workspace, Nix product
derivation, macOS, AArch64, Windows, benchmarks, or cross-process compiler-to-baseline
lifetime stress. The lock tests prove coherent baseline reads, not every concurrent
compile/load ordering. Scripts outside the workspace and the existing Cargo handoffs when
script output changes remain limits of the hybrid path; this repair does not certify the
repository's complete no-rustc goal. These are acceptance limits, not successful lanes.

The source operations used unique function and constructor anchors and inspected their
call sites. Mutation preview was skipped because this worktree has no project index;
all edits completed without an interrupted mutation. The final diff was checked for
unnecessary comments, defensive wrappers and unused code.

## Review update, 2026-10-28: baseline capture is locked; the lane stays parked

The latest review found that `current` held Cargo's locks but `Run::of`, which captures
the baseline, did not. It could combine the previous run's printed output with a directory
being changed by Cargo, or accept the partial directory of an interrupted run. `Run::of`
now takes the same nonblocking locks, verifies the completed fingerprint, and holds the
locks until both the printed output and directory digest have been read. A busy directory,
a missing lock, and an unfinished fingerprint all refuse baseline capture.

Three focused regressions failed on the previous implementation and pass with the fix.
On Linux with Rust 1.99.0, a small test crate compiling the actual `script.rs`, `records.rs`
and `dep_info.rs` with the real `blaze-build-script` dependency passed 50 tests; the existing
measurement test stayed ignored. This is component validation, not a full `ra-jit` build.
The comparison harness self-test, 32 release-policy cases, 20 Nix workflow checker cases,
138 harness imports, formatting, the compiler-site audit (15 existing sites, none new),
and `git diff --check` passed. The merged Nix graph was regenerated using Cargo 1.99.0
metadata on Linux and its input hashes match the manifests and lockfile.

The branch now includes `dist/main` at `a5207402`. Conflict resolution preserves the
owned-script scheduler and leases, and the removal of cached script-run replay. An
independent source review caught a missing test-fixture initializer during that integration;
both new fields are initialized. The shared release checker also rejects parent traversal
in a supposedly store-bound Nix loader path.

**PR #5 remains parked and is not ready to merge.** A failed or rejected hybrid script run
can still be executed again when control returns to Cargo. This repair does not establish
the full compiler-to-baseline lifetime invariant or supersede the earlier live-Cargo
acceptance failures. The active owned-script implementation from #12 remains the direction
for execution. No full compiler, LLVM, macOS, Windows, Nix derivation or end-to-end hybrid
acceptance was run in this review. The measurements below are historical evidence from
the original work, not measurements of this update.

Unit u8-buildscript of the distributed compile, cache and tests program. The question:
[TRACEDECAY.md](TRACEDECAY.md) (items 4 and 7) found that 18 of 18 builds after an edit went to cargo
because "the build script of `tracedecay-cli` printed something else" the second time blaze ran
it, and guessed the cause was the script reading git's state (`build.rs:417-422`, **unverified**).
What is it, and which side is wrong, blaze or tracedecay?

**read** means a number or line was read from a run here, with the run count. **inferred** means
it follows from what was read. Nothing is extrapolated.

## The short version

**blaze is wrong, and tracedecay's script is fine.** The cause is not git's state.

- **What differs.** When the build script's output is compared, blaze runs the script again into
  an **empty `OUT_DIR`**, where cargo runs it in the **same `OUT_DIR`**, with the frontend bundle the
  first run left there. In an empty directory the script has no record of what the bundle was built
  from, so it runs `pnpm run build`. rsbuild prints 61 lines to the script's standard output
  (`Rsbuild v2.2.13`, `ready built in 0.77s`, a table of 52 staged files with their sizes). Cargo
  reads only lines that start with `cargo:`; the others are the script's own and it ignores them.
  blaze compared every byte, against the output cargo kept from its own last run, which has none of
  those lines. 55 of the 61 lines differ from the first run's as well: the time, the path (it names
  the `OUT_DIR`, relative to `dashboard/`, and so is not the path blaze rewrites) and the width
  of the table's first column, which follows the path.
- **Cargo has the same lines, once.** Cargo's first run of the script prints the 61 lines. Its
  reruns print none, and are byte for byte the same as each other (3 reruns, 877,830 bytes each).
  The script's output depends on the state of its `OUT_DIR`, which cargo leaves as it was between runs
  (**read**: the first rerun found the bundle and did not run pnpm).
- **Fix: in blaze** (`crates/ra-jit/src/cargo/script.rs`). The daemon runs the script in the
  `OUT_DIR` cargo left, under cargo's lock on its build directory, when cargo's own records say
  cargo would run it again, records the run as cargo records its own (`records.rs`), and compares
  the lines cargo reads, as cargo reads them, and what it wrote with the run the build had.
  (The first two versions ran it in a copy of the directory, which two reviews failed on what a
  copy does not keep; "Round 3" is why it is now cargo's own directory, and "Round 4" why the run
  is cargo's run, so that a script runs once for a change, as under cargo.) With the
  fix the scratch project that has tracedecay's own `build.rs` goes from 0 of 3 edit builds the
  daemon's to 3 of 3 (read, 1 run each of `blaze build`), and the harness case that has the script in
  miniature from 0 of 3 to 3 of 3. tracedecay needs no edit; an optional patch that keeps pnpm's lines off the
  script's standard output is in the last section.

## What was run

| | |
|---|---|
| Machine | Apple M5 Pro, 18 cores, 48 GiB, macOS 26.6.2. The machine was not quiet: other units' builds ran (load average 7.4 to 36.7 at the start of the runs) |
| blaze before | the fork at `c8e5f6a8` (`/Users/bytedance/dev/blaze-targets/u1/release`); `dist/main` (`a67ea8db`) differs from it by docs and `harness/tracedecay.py` only (`git diff c8e5f6a8 origin/dist/main --stat`) |
| blaze after | this branch, `cargo +1.99.0 build -p ra-jit --release`, 2 m 10 s (1 run), in `/Users/bytedance/dev/blaze-targets/u8/release` |
| tracedecay | `d79de82273` in a detached worktree of the owner's checkout, `td-u8`, with `#![recursion_limit = "256"]` in `tracedecay-code-index-runtime` as in [TRACEDECAY.md](TRACEDECAY.md), `pnpm install --frozen-lockfile --offline` run in it. Nothing was written in the owner's checkout |
| The script | the unchanged `crates/tracedecay-cli/build.rs` and `build-support/`, compiled by cargo 1.99.0 as the build script of a scratch package, `td-u8/local-only/u8bs` (`local-only/` is gitignored, and the script finds the repository at `../..` from its package, as for the CLI). Its build dependencies are the CLI's (`flate2`, `serde_json`, `sha2 0.11.1`). Its `[package]` has no dependencies, so nothing but the script is built. The whole of tracedecay does not fit this Mac under blaze ([TRACEDECAY.md](TRACEDECAY.md) item 5) and is not needed to see the script |
| Rust | 1.99.0 for cargo and blaze |

Every build of blaze, every run of the build script and every blaze daemon run held the heavy lock (the compile of
the scratch package's build script, with a `pnpm` that fails at once, and `pnpm install` did not); every blaze run had `RA_JIT_MEMORY_MAX=16G`,
`RA_JIT_OWN_BASE_MEMORY=12G`, `RA_JIT_SPARE=0` and the harness watchdog (24 GB).

## 1. The two outputs

Cargo runs the script, then runs it again after a tracked file of the repository has changed (the
script watches every file `git ls-files` lists, 6,734 of them here). One cargo 1.99.0 build per
step, `cargo build --offline`, into one target directory; `output` is the file cargo keeps beside
`OUT_DIR` (`E1`, 1 run of 4 builds):

| Step | `output` of the script | Lines | Lines that do not start with `cargo:` |
|---|---|---|---|
| 0, cold build | 887,181 bytes | 6,801 | **61** |
| 1, after an edit | 877,830 bytes | 6,740 | 0 |
| 2, after another | 877,830 bytes | 6,740 | 0 |
| 3, after another | 877,830 bytes | 6,740 | 0 |

The 6,740 lines of steps 1 to 3 are 6,734 `cargo::rerun-if-changed` and 6 `cargo::rerun-if-env-changed`.
The script prints no other directive: nothing for rustc. The `cargo:` lines of step 0 are the same
6,740, in the same order (`cmp`).

The 61 lines of step 0 are the output of `pnpm run build` (rsbuild), which the script starts with
`Command::status()`, which inherits the script's standard output (`build.rs:292-298`, the `run_pnpm`
function):

```
Rsbuild v2.2.13

info    build started...
ready   built in 0.77s

File (web)                                  Size        Gzip
../target/u8-e1.noindex/debug/build/u8bs-1e11a47fc7e7b31a/out/dashboard-bundle/staging/index.html   0.55 kB     0.30 kB
... 51 more rows ...
                                            Total:   3533.6 kB   1236.0 kB
```

Now blaze's side. The daemon takes the environment (103 variables) and the directory cargo ran the
script with (`ra-jit-ran.json`, written by `ra-jit build-script`), sets `OUT_DIR` to an empty
directory beside cargo's and runs the script (`rerun_into`). The two reruns below are the script run by
hand with the environment cargo gave it at step 2 (`E2`, 1 run each), compared with cargo's `output`
after step 3:

| Rerun | Exit | Time | Standard output against cargo's `output` | `OUT_DIR` against cargo's |
|---|---|---|---|---|
| in an empty `OUT_DIR` (what blaze does) | 0 | 1.8 s | **different**: 887,883 bytes, 61 lines that are not directives; 55 of them differ from step 0's | same digest (125 files) |
| in a copy of cargo's `OUT_DIR` (what cargo does) | 0 | 0.9 s | **the same bytes** (877,830) | same digest |

The 55 lines that differ from step 0's, as `diff` prints them, are in three places: `ready   built in
0.77s` against `built in 0.51s` (the time); the table's header and each of its 52 rows, because rsbuild
prints the staged path relative to `dashboard/` (`../target/.../out/dashboard-bundle/staging/...` against
`.../ra-jit-out-fresh/dashboard-bundle/staging/...`), which blaze's rewrite of its directory's absolute
name does not match; and the `Total:` line, which is padded to the width of the longest path. The sizes
are the same, and so is the bundle (digest `771c39c0...`, the same in `td-u8` and in u5's `td-u5` cold
build, read from its `out/dashboard-bundle/`).

## 2. Why they differ

| Candidate | Same under blaze's rerun and cargo's? | Evidence |
|---|---|---|
| Environment | Same: blaze runs the script with the variables cargo gave it (103; `env_clear` then `ran.env`). The jobserver in `CARGO_MAKEFLAGS` points at descriptors the daemon does not have; the rerun does not need them (**inferred** from its exit 0 and its output) | `ra-jit-ran.json`; the rerun above exits 0 |
| Working directory | Same: `ran.cwd`, the package's directory | `ra-jit-ran.json` |
| Git state | Same: the edit does not add or remove a file, so `git ls-files` is the same list; the tree was dirty before and after (`recursion_limit`); no untracked file appeared after any step (`git ls-files --others --exclude-standard` empty, 5 reads). `PRODUCT_SOURCE_DIRTY` is in the generated module, which is a file, not output | step lines of `E1` |
| Time | Differs, in the one line `ready built in Ns` | 1 line |
| **`OUT_DIR`** | **Differs: empty for blaze, the first run's for cargo.** The script keeps `dashboard-bundle/built-from` there (the fingerprint of the frontend's inputs) and builds only when it is not what the bundle was built from (`build.rs:217-221`) | the 61 lines are there for the empty directory and not for a copy |

So the difference is `OUT_DIR`, and the time and the path in the lines are two of the ways the 61 lines
differ from step 0's. What git's state does to this script is different and is not the cause: it
changes the file list, and so the `rerun-if-changed` lines, and neither the edits nor the reverts of
[TRACEDECAY.md](TRACEDECAY.md) do that.

Why 18 of 18 and not some: cargo's last run, the one blaze compared with, was in the same directory as
the bundle, so after the first rerun its output never has the 61 lines, and blaze's run in an empty
directory always has them. Each edit that blaze sent to cargo had cargo rerun the script in its own
directory, which left `output` without the lines again. The first edit after a cold build (whose
`output` has the 61 lines) differs from the daemon's run in them too, by the time and the paths.
Run A of [TRACEDECAY.md](TRACEDECAY.md) item 3, which counted the message once during a cold build,
is consistent with the same mechanism (the check runs when a watched file was written after cargo's
run; **inferred**, its log is gone); why run B had none is **not explained**.

## 3. Is the script nondeterministic under cargo too?

Its first run is, in lines cargo ignores: the 61 lines have the time and the paths. Its reruns in the
same `OUT_DIR` are not (3 reruns, the same 877,830 bytes, `cmp` equal), and nothing cargo reads differs
between the first run and the reruns (the 6,740 `cargo:` lines, `cmp` equal). Cargo does not compare
the output of one run with another's at all: it runs the script when a path it was told to watch is
newer than the last run, and uses the new output. blaze's check asks something cargo never does
("is it the same as last time?"), and it asked with a stricter reading of "the same" than the one cargo
has of what a script prints.

## 4. The fix

In blaze, because both halves of the difference are blaze's:

1. **Run the script in a copy of cargo's `OUT_DIR`**, as cargo does. (This step is replaced: the
   script now runs in cargo's own `OUT_DIR`, under cargo's lock, and is compared with a record of
   the run the build had; see "Round 3".) `rerun_matches` copies the
   directory (files and directories with their times and permissions; see "After review" for the
   links) to the directory it runs the script in, so a script that keeps what it built there finds
   it. cargo's directory is not written to, which is why it was a directory of its own (compilers
   under way read cargo's; cargo builds the package again when a file there is newer). Copying
   `td-u8`'s 125 files, 4.5 MB, took under a second (the cost of the reviewed version is in "After
   review").
2. **Compare the lines cargo reads.** `directives()` keeps the lines that, as text and trimmed,
   start with `cargo:` (`cargo::` too), which is where cargo's `BuildOutput::parse` starts (and
   [`blaze-build-script`'s `output::parse`](../../crates/blaze-build-script/src/output.rs)). What the script
   or a tool it runs says besides is the script's own. `rerun_into` and `same_run` compare those, in order.
   What the script wrote into `OUT_DIR` is still compared whole (`tree_digest`, which is `digest_of`
   of the first version, made to read links and failures as "After review" says), so a script
   whose files change still sends the build to cargo.

Neither step makes a build the daemon's that cargo would not have built the same. A script that prints
a different directive (a `rustc-cfg`, `rustc-env`, a link argument, another `rerun-if-changed`), or
writes a different file, is still cargo's, with the same message.

## 5. Checks

### The scratch project under `blaze build` (E3)

`blaze build --locked -p u8bs` in the scratch package, cold, then three edits (a tracked file of
`td-u8` and the package's `lib.rs`, so that the daemon has something to compile and the script's
watched files are newer), under the guard and the watchdog, 1 run of each (n=1; the harness's records are
in `u8-work/e3-before` and `e3-after`):

| | cold | edit 1 | edit 2 | edit 3 | `blaze status` afterwards |
|---|---|---|---|---|---|
| fork `c8e5f6a8` | the daemon's, 113.0 s | **cargo's** 12.7 s | **cargo's** 41.4 s | **cargo's** 6.4 s | `1 builds by the daemon, 3 by cargo`, `3 x the build script of u8bs printed something else` |
| this branch | the daemon's, 62.1 s | **the daemon's** 1.62 s | **the daemon's** 1.81 s | **the daemon's** 1.55 s | `4 builds by the daemon, 0 by cargo` |

The first line of each of the fork's three edit logs is `ra-jit: bug: the build script of `u8bs`
printed something else; building with cargo, which ra-jit is never to do (no-rustc M, #93, and #84)`,
word for word the line of the 18 edit logs of [TRACEDECAY.md](TRACEDECAY.md). With the fix the logs
have no such line and end `Finished `dev` profile [ra-jit]`. The wall times are not a comparison of the
two blaze builds: the fork's edits were cargo's builds (with the daemon's workers loading
again) and ran while other units' builds loaded the machine (load average at their start 16.8, 14.9
and 11.6, against 8.0, 7.9 and 7.4 for the branch's; no other build in the edit runs themselves, peak
0.0 cores of other builds); the point is who built them.

### Unit tests

`cargo +1.99.0 test -p ra-jit` (debug, 1 run, 2 m 2 s with the build): **197 passed, 0 failed**, in the
`ra-jit` binary's tests; the other targets have none. That is the 193 of `dist/main` and the 4 added in
`cargo::script::tests` (**inferred** from the names; `dist/main`'s own count was not run). The 9
script tests that were already there pass with the change. The two tests that run a script
(`a_rerun_finds_in_its_directory_what_the_last_run_left_as_cargos_does` and
`what_a_script_says_that_cargo_does_not_read_is_not_compared`) **fail on the old comparison**:
with the three lines of `rerun_matches`, `rerun_into` and `same_run` put back (a rerun into an empty
directory, `bytes == bytes`), both stop at `left: Err("printed something else")` (`cargo test -p ra-jit
--bin ra-jit -- cargo::script`: 11 passed, 2 failed). The other two new tests (`directives`, `copy_tree`)
test the functions the change adds and cannot run on the old code.

### The harness case (`harness/scriptrerun.py`)

A project with a build script of the CLI's kind (it hashes `inputs/`, records the hash beside the
bundle it writes into `OUT_DIR`, runs a tool that says how long it took, which process it was and
where it wrote, and lists every file as `rerun-if-changed`), built with `cargo ra-jit build`, under
the guard and the watchdog (peak 2.46 GB), 1 run of each:

| Step | fork `c8e5f6a8` | this branch |
|---|---|---|
| an edit of the program | cargo's: the build script of `app` printed something else (**WRONG**) | the daemon's |
| an edit of a file the script watches | cargo's: printed something else (**WRONG**) | the daemon's |
| another edit of the program | cargo's: printed something else (**WRONG**) | the daemon's |
| an edit of an input the script bundles | cargo's: printed something else; the program prints the new bundle | cargo's: wrote something else; the program prints the new bundle |

The program's output is cargo's in every step of both runs. The last step is cargo's on purpose: the
script writes another bundle, and the daemon does not build what a script wrote anew. With the fix
the run exits 0 in 13.8 s; against the fork it exits 1 after the three steps (23.8 s). The harness is in
`harness/ci.py` (part `core`, quick tier 1, Linux and macOS) and `docs/harness.md`; `harness/imports.py`
imports it (119 of 128 scripts import on this Mac's Python 3.9; the nine that do not need `tomllib` or
a newer syntax and are not this change's).


## 6. The optional patch for tracedecay

None is needed. The script follows cargo's contract: cargo reads the lines that start with `cargo:`
and leaves the rest alone, and it runs the script in the same `OUT_DIR` every time. A check that
compares everything a script prints, from a directory cargo never gave it, asks for more than the
contract says. blaze is the one to change, and did.

One thing tracedecay could do is still worth saying, because it makes the script's standard
output hold what cargo reads and nothing else (a build script's standard output is the channel to
cargo, and pnpm's lines on it are noise there). `run_pnpm` starts pnpm with `Command::status()`,
which has pnpm inherit the script's standard output. Capturing it and writing it to standard error
keeps the lines for a person (`cargo build -vv` shows a script's standard error, and a failed
build prints it) and makes the first run's output the same as every rerun's. This patch,
against `d79de82273` (`crates/tracedecay-cli/build.rs`), was applied in `td-u8` and run as E1 was
(cold build and 3 reruns, then both reruns of blaze's check; E4, 1 run, since reverted):
the cold output was 877,830 bytes with 0 lines that are not `cargo:` (887,181 and 61 without it),
and blaze's old rerun in an empty `OUT_DIR` printed the same 877,830 bytes, in 1.6 s, with the same
`OUT_DIR` digest. So with this patch even the fork's check at `c8e5f6a8` would have passed on this
script. It is not the fix: the next script that keeps its tool's noise on standard output would
meet the check again, and the old check ran the frontend's tool again each time (1.6 s here against
0.8 s for the rerun in a copy; the CLI's dashboard is built in under a second on this Mac,
`ready built in 0.77s`).

```diff
--- a/crates/tracedecay-cli/build.rs
+++ b/crates/tracedecay-cli/build.rs
@@ -295,11 +295,11 @@
             .args(args)
             .envs(envs.iter().copied())
             .current_dir(dir)
-            .status()
+            .output()
     };
     // Standalone pnpm installs `pnpm.exe`; npm and Corepack install a
     // `pnpm.cmd` shim, which `Command` does not resolve on its own.
-    let status = match spawn("pnpm") {
+    let output = match spawn("pnpm") {
         Err(error) if cfg!(windows) && error.kind() == io::ErrorKind::NotFound => spawn("pnpm.cmd"),
         result => result,
     }
@@ -310,6 +310,17 @@
             args.join(" ")
         ))
     })?;
+    // A build script's standard output is Cargo's: it reads the lines that start
+    // with `cargo:` and keeps the rest unread. pnpm's own lines (how long the
+    // bundler took, the staging paths it wrote) differ on every run, so they go to
+    // standard error with the rest of what pnpm printed.
+    {
+        use io::Write as _;
+        let mut stderr = io::stderr().lock();
+        let _ = stderr.write_all(&output.stdout);
+        let _ = stderr.write_all(&output.stderr);
+    }
+    let status = output.status;
     if status.success() {
         Ok(())
     } else {
```

## What was not measured

(Of the first version. The items on the copy, its size and its platforms no longer apply: "Round 3".)

- **The whole of tracedecay under blaze.** It does not fit this Mac ([TRACEDECAY.md](TRACEDECAY.md),
  items 5 and 7). The script, its environment and its files are tracedecay's; the daemon's load of
  the 586 crates, the 107 build scripts it fails to compile on macOS arm64 and what a loaded daemon
  does with an edit of `tracedecay` or `tracedecay-mcp` are not run here. With the fix, **the
  first reason** the 18 edits went to cargo is gone; whether the daemon then builds each of them
  depends on the load finishing within the machine's memory (item 7, gaps 1 and 2) and is **unverified**.
- **The gate.** `harness/ci.py core` (the other harnesses), the LLVM backend, `cargo test --release`
  of the other crates and `cargo fmt --all -- --check` under CI's toolchain were not run; the last was
  run with 1.99.0's `cargo fmt` and passes. Only `script.rs` of any crate changed.
- **Linux.** The scratch project, E1 to E4 and the new harness ran on macOS only. The fix has no platform code beyond
  the copy of links, which Windows leaves to cargo, and of directories, whose times Windows does not
  let the copy set (`File::open` of a directory), so a rerun there is cargo's too (**inferred**, not run on Windows).
- **A script that writes the name of its `OUT_DIR` into a file** is still told apart from cargo's run
  (`wrote something else`), as before: the copy of the directory has the old name in it and the script
  writes the new one. Whether the CLI's does: the 125 files of its `OUT_DIR` have the same digest, so it does not.
- **Run A and run B of [TRACEDECAY.md](TRACEDECAY.md)** (why the message once appeared during a cold
  build and once did not): the u1 target directories are gone.
- **A size limit on the copy.** A script whose `OUT_DIR` holds gigabytes (a C++ library built there)
  is copied in full at each check, and read three times (see "After review" for the cost at 4.5 MB). The
  cost at that size was not measured, there is no limit and no time limit on the script's run, and a
  run in an empty directory builds that library again, which is more.
- **Cost of the daemon's other work on this script.** `ra-jit build-script` took over a minute after
  the CLI's script ended in the cold build of E3, in `Kept::of` hashing each `rerun-if-changed`
  directory and file for the store (one stack sample of the running process: 602 of 785 samples in
  `digest_of`, 160 in `watched_digest`; **read**, n=1, load average 24). It is not part of this fix
  and is a follow-up.

## After review (unit u8-fix)

A verifier (GPT 6.1; its report is not in the repository) failed the first version of this fix
with three findings in `script.rs` and a weak gate in `harness/scriptrerun.py`. All four are
real and are fixed on the same branch. Each has a test that fails on the first version and
passes now: `cargo +1.99.0 test -p ra-jit --bin ra-jit -- cargo::script` on `182389d8`'s
`script.rs` with the new tests added and nothing else gave **18 passed, 10 failed** (1 run), and
the 10 are the tests named below as failing before (one of them is a hang that the test gives up on
after 20 s); on this branch the same tests give 29 passed, 0 failed, 1 ignored (see "How the tests
were run").

| # | Finding | What the first version did | Now | Test (fails before) |
|---|---|---|---|---|
| 1 | Times and permissions | Directories were created new: a script that tests whether a directory is older than an input (`make` style) found it new, did not make its output again, and the rerun was "the same". The mtime of a file was set best effort: a read-only file lost it on Linux (**inferred** from the code, which ignored the error: not run on Linux) | Every directory is given the times and permissions of the original after what is in it (a read-only directory is made writable again only to remove the copy), every file is copied through one handle and given its times and permissions on it, and a failure of any of this is an error: the run is cargo's | `a_directory_of_the_copy_is_as_old_as_cargos_is`, `a_directory_of_the_copy_has_the_permissions_of_cargos_and_is_removed_with_them`; `a_read_only_file_of_the_copy_is_as_old_as_cargos_is` passes on the first version **on macOS** (`fs::copy` keeps a file's time there) and is the case for Linux |
| 2 | Links | A link was copied as it was unless it was an absolute path below the directory: `../state`, an absolute path out of the directory, and `../out/data` (the original by another name) all led from the copy to cargo's own files, so a script wrote there, and the comparison, run after, found both sides the same | Each link is resolved (`canonicalize`). If it ends below the directory it is made a link to the same place below the copy; if it ends anywhere else, or nowhere, or the links go round in a circle, the copy is an error and the run is cargo's. A file that is no file, directory or link (a pipe) is an error too: the copy waited for a writer. The digest before the run is taken from the copy before the script starts, never after, and cargo's directory is read after the run too, for its contents and (a `stat` of everything in it, no read) its sizes and times, and has to be as it was, so a write that reached it by a way not seen is found, one of the same bytes too, which makes its file newer. The digest does not follow links (a link to the directory itself was an endless tree), records where each one leads, and fails on what it cannot read, where it read nothing and called that equal | `a_link_that_leads_out_of_the_directory_is_not_written_through` (relative and absolute), `a_link_that_leads_back_in_by_another_name_is_written_through_in_the_copy`, `a_link_that_leads_nowhere_or_in_a_circle_is_not_run_through`, `a_link_the_script_points_elsewhere_is_a_difference`, `what_the_script_left_is_compared_with_what_was_there_before_it_ran`, `cargos_directory_is_not_written_to_even_with_what_it_has`, `a_file_that_is_not_a_file_is_not_copied_for_a_run`; kept as controls that pass on both: `links_inside_the_directory_are_links_to_the_copy`, `a_link_to_the_directory_itself_is_fine` |
| 3 | Lossy decoding | `String::from_utf8_lossy` ran before the lines were picked, so a line with a byte that is not UTF-8 (`KEY=\xff`) became `KEY=U+FFFD`, equal to a valid old line, where cargo skips the whole line and has no `KEY` | The lines are picked from the bytes, as `BuildOutput::parse` does (cargo `src/compiler/custom_build.rs`, 933 to 961 at `5f94df478`, the reference the verifier read): a line that is not valid UTF-8 is skipped whole, a valid one is trimmed, `cargo:` and `cargo::` both count, order and repeats are kept; the name of the directory is replaced in the lines picked, so in both callers (`rerun_into`, `same_run`) | `a_line_that_is_not_text_is_not_a_line_cargo_reads_in_either_run` (both callers), `only_the_lines_cargo_reads_are_directives`, `the_name_of_the_directory_a_script_ran_in_is_replaced_in_the_lines_cargo_reads` (these last two use `directives_named`, which the first version lacks, and cannot run on it) |
| 4 | The gate | The harness compared the program's standard output only, so a program that exited differently, or wrote to standard error, or a reference `cargo run` that failed (both print nothing), passed | `differences()` compares exit status, standard output and standard error (as `program_output` of `cargo_ra_jit.py` reads them), and fails when the reference run failed. `selftest()` runs first, and alone with `--selftest`, over six cases, 4 of them the old check passed | `python3 harness/scriptrerun.py --selftest`; the old check (`ran.stdout != theirs.stdout`) misses 4 of the 5 wrong cases, the new one finds all 5 (run here) |

The verifier's missing-directory and failed-script cases (`a_directory_that_is_missing_is_an_error_and_leaves_no_copy`,
`a_script_that_fails_is_an_error_and_leaves_no_copy`) pass on both versions; they stay as tests of the
clean-up, which now looks for any `ra-jit-out-*` beside the directory (the copy's name has a counter
now, so two checks in one process do not share a directory). One existing test,
`a_rerun_leaves_what_cargo_has_alone`, took its time for "a second ago" after the check it was
timing and failed when the machine made the check take more than a second; it takes it before.

### Numbers

| | Count |
|---|---|
| `cargo +1.99.0 test -p ra-jit` on `182389d8` (the verifier's run) | 197 passed, 0 failed (read from the verifier's report; not re-run here) |
| the same on this branch | **213 passed, 0 failed, 1 ignored** (1 run, debug; the ignored one is the measurement below). `cargo::script::tests`: 13 before, 29 run and 1 ignored now |

**Cost of the copy** (n=10 per row, one session, an Apple M5 Pro that was not quiet: load average 3 to 5
and other units' processes; release build; `cargo test --release -p ra-jit --bin ra-jit -- --ignored
--nocapture the_cost`). The directory is **not** tracedecay's: its `td-u8` worktree is gone, so it is
a made-up one of the size the first version of this report measured for it (125 files, 4,530,000
bytes in 7 directories, random bytes), and the script does nothing, so the figures are the cost of
the check and not of a script. `rerun_is_same` as a whole, the first version and this one measured
in the same session, two rounds each:

| | min | median | max |
|---|---|---|---|
| first version (`182389d8`), round 1 | 515 ms | 603 ms | 1,000 ms |
| first version, round 2 | 589 ms | 618 ms | 1,206 ms |
| this branch, round 1 (before the look at cargo's sizes and times was added) | 259 ms | 357 ms | 641 ms |
| this branch, round 2 (the same) | 169 ms | 341 ms | 508 ms |
| this branch, final code | 178 ms | 361 ms | 576 ms |

By step on this branch (final code): copy 55 ms median; hash of the copy before the run 250 ms (min
85, max 343); the script 5 ms (it does nothing); hash of the copy after 13 ms; hash of cargo's 14 ms;
removing the copy 26 ms; the `stat` look at cargo's, twice, is in the whole and was not timed alone. The limiter is the first read of a file that was just written, which costs
about 1.5 to 2.5 ms a file here and 0.1 ms after the first (a scratch program, 125 files, 10 runs: reopening a
copied file by path to set its time cost 330 ms, setting it through the handle that wrote it
36 ms for the whole copy). That is why the files are copied through one handle. What on this Mac
does it was not isolated; the machine runs endpoint-security extensions, which is a **guess**. Linux was not measured. The
first version's cost is, by this, mostly that reopening (**inferred**; its steps were not timed
alone), and this one is cheaper than it was though it reads the tree three times (before, after,
and cargo's) where the first read it twice. A directory of
gigabytes is not measured and has no limit; it costs a copy, three reads and a removal at each check.

Not done: a limit on the size of the copy or the length of the script's run (the verifier's note,
not a finding); hard links inside the directory are copied as separate files, so a write through
one name that the script then reads by another is in the copy as two (not tested: **inferred** that
the comparison still finds a difference when the script writes anything different); a script that
finds cargo's directory by its own path (`$OUT_DIR/../out`) writes there, which the copy cannot
prevent, and the look at cargo's after the run finds it and makes the check an error, after the
write (`cargos_directory_is_not_written_to_even_with_what_it_has`); the full tracedecay run is still
not made (this Mac, as above). Windows: the copy of a directory's times and of links is cargo's
there (**inferred**, not run).

### How the tests were run

Every run held the heavy lock (`with-heavy-lock.sh`) and the guard (24 GB sum of top MEM, swap
growth), `CARGO_TARGET_DIR` of its own, `RA_JIT_MEMORY_MAX=16G RA_JIT_OWN_BASE_MEMORY=12G
RA_JIT_SPARE=0`, `RUST_BACKTRACE=0`; peaks 0.9 GB (unit tests), 1.9 GB (release build), 2.6 GB
(harness). No swap growth in any run. "The first version" for the failing run is the new tests added
to `182389d8`'s `script.rs` and nothing else (the ones that use only `rerun_is_same`, `rerun_matches` and
`same_run`, which that version has); the cost rows are the first version's `script.rs` with a
timing test added, and then this branch's, restored byte for byte (`sha256` checked). The harness:
`RA_JIT_BIN=.../release/ra-jit python3 harness/scriptrerun.py` on this branch's release build
passes (the first three edits the daemon's, the fourth cargo's with "wrote something else", the
program as cargo's in all four, its exit status and standard error too).

## Round 3: the premise (unit u8-premise)

A second verifier run (GPT 6.1, round 2) failed `88f76038` on three more ways the copy was not
cargo's directory: a link to a link was made a link to where the last one led, so a script that
points the middle one elsewhere and back found another file than under cargo; extended attributes
were not copied (macOS); and two names of one file became two files. Each kept old generated data
where cargo's run made new (**read** from the verifier's report: its tests returned `Ok` with
`data` still `old`, and cargo's run in the same directory wrote `new`). Round 1 had failed the copy
on fresh directory times and on links that wrote into cargo's files. Every fix so far assumed that a
copy of `OUT_DIR` can stand in for the directory cargo runs the script in again. A copy cannot be
that in full: the time a file's inode changed, inode numbers, hard links, file flags, ACLs, sparse
files and where each link in a chain leads are all things a script can read, and a copy keeps some
of them on some systems. So this round tested the premise before writing a fix.

### Why the rerun was not in cargo's directory

The rerun has been in a directory of its own since the code was imported from rustd (`fb515039`).
The reason is in the doc of `rerun_is_same` there, word for word: "what cargo's holds is read by the
compilers of a build under way, and cargo compiles the package again when a file there is newer
than it was" (**read**, `git show fb515039:crates/rustd/src/cargo/script.rs`). Read against the
callers on this branch, the first half is true and the second is not a reason:

- **Readers and writers at the same moment: true.** Three places check a script. (A) `cargo ra-jit`,
  before it decides whether cargo builds (`cargo/mod.rs` `by_daemon` calls `State::broken_guard`), with no
  lock held: a cargo in another terminal can be building in the same directory. (B) The rustc wrapper
  under plain cargo (`cargo/wrapper.rs` `Unit::by_daemon`, the same guards), which runs inside cargo's
  build: that cargo holds the lock on its build directory (`.cargo-lock`) and can be running this
  script or compiling a crate that `include!`s what it wrote, at that moment. (C) The daemon
  (`inputs.rs` `Script::reruns_the_same`), asked by either. Nothing locked `OUT_DIR` for the check.
- **A file made newer: no build cargo would not make.** A check runs only when something the script
  reads was written after cargo's last run of it. Cargo's next build then runs the script again
  whatever blaze did, and compiles the package again whatever the script wrote (`fingerprint` of the
  run unit; the module doc of `script.rs`). The daemon's own guards leave `OUT_DIR` out on purpose
  (`cargo/state.rs` `guards`: "only cargo writes it again: when the script runs again, which its own
  guards are for"), and `symbols.rs` compares the files there by their bytes, not their times.

So what a check must not disturb is: `OUT_DIR` while cargo runs the script or a compiler reads it, and
what cargo keeps of its last run (`output` or `run/stdout`, `invoked.timestamp`, the fingerprint),
which is what has cargo run the script again.

### In place is cargo's own sequence, under two conditions

(This premise was wrong for a script that is not idempotent: blaze's run, then cargo's, is two runs
where cargo alone makes one. "Round 4" records blaze's run as cargo's instead.)

Cargo 1.99 runs a script so (`src/cargo/core/compiler/custom_build.rs` `build_work`, the verifier's copy
of `5f94df478`, **read**): it creates `OUT_DIR` if it is not there and never empties it, stamps
`invoked.timestamp` before it starts the script, and after the script exits writes `output` with the
time it started as its time; the fingerprint is written when the unit is done. A run of blaze's in
`OUT_DIR`, with the environment and directory cargo ran the script with, that writes none of cargo's
files, leaves cargo as a build that stopped right after the script exited does: `OUT_DIR` from a whole
run with today's inputs, and cargo's records of the run before. Cargo's next build runs the script
again (an input is newer than `output`), in the directory the last run left, and compiles the
package. A script already has to work that way: cargo runs it again for every touch of a file it
watches, and after any build that stopped. Blaze's run, then cargo's, is what cargo does for an
edit, a build, a touch and a build (**inferred** from the code read; not run against cargo).

Two conditions make it so, and the code has both:

1. **The exclusion cargo's run has.** The check takes cargo's lock on the build directory
   (`.cargo-lock` above `OUT_DIR`, as `State::hold` does) without waiting: a cargo that waits for the
   wrapper it is running would wait for ever. While a cargo holds it (B, and C when B asked), blaze
   runs nothing. It reads cargo's own last run when cargo's own rule says cargo would not make it
   again (nothing the script reads is newer than `output`): cargo then ran the script in this build
   and is done with the directory. Otherwise the build is cargo's ("was not run again: a cargo is
   building in its directory"). The daemon's own reads of `OUT_DIR` take no lock; they are as open to
   blaze's run as to cargo's own reruns, which they are today.
2. **A record of the run the build had.** With a copy, cargo's directory stayed what the build was
   made from. In place, a run that differs changes `OUT_DIR`, and when the build after it never comes
   (cargo fails or is stopped first), a later check would compare its run with the changed directory
   and find them the same. So the run a build had is recorded when the build is made: in the state
   (`Guard::Script.built`, by `State::after`) and in the load (`Script.loaded`), as the lines cargo reads
   and a digest of `OUT_DIR`, and a run is compared with that. This also closes a hole both earlier
   versions had: under plain cargo, cargo runs the script again in the build before the wrapper asks,
   and the check compared a rerun with cargo's new output, which says whether the script is
   deterministic, not whether it printed what the build the state is of had (**inferred** from the code;
   no test of the old version was written for it).

What blaze's run printed is kept beside the directory in `ra-jit-checked`, which it already wrote for
its time; its time is still when the run began. The last run of the script is cargo's or blaze's,
whichever began later, and when nothing the script reads changed after it began, no run is made
(`current`). That takes the place of the shortcut `inputs.rs` had for a run `cargo ra-jit` made just
before it asked the daemon (`checked_after` and `same_run`, deleted).

### Alternatives not taken

- **A clone of the tree** (`clonefile` on APFS, `FICLONE` on Linux). It keeps more of each file, but
  two names of one file are still two files, `FICLONE` is per file so a directory is a walk again with
  its own metadata to set, and an absolute link still leads into cargo's directory. It narrows the
  list; it does not end it.
- **Leave to cargo any `OUT_DIR` with links, hard links or extended attributes.** That is the copy with a
  list of what it refuses, which each review lengthens (the time a file's inode changed, inode numbers,
  flags and ACLs are not on it), and a script that leaves a link would send every edit to cargo again.

### What was deleted

The copy and everything that was for it: `copy_tree`, `copy_below`, `copy_file`, `times_of`, `copy_link`,
`remove_copy` and the counter that named the copies; `rerun_matches` and `rerun_into` (a run in a copy
and its comparison); `same_run` and `checked_after` (the shortcut above); `rerun_is_same` (now
`current` with the comparison at the caller); the second kind of digest (`Look::Times`, sizes and times
of cargo's directory, read after the run to find a write that missed the copy); and `led_to`, which
resolved links for the copy. The code of `script.rs`, without its tests, is 936 lines on `88f76038`
and 873 here (`wc -l` up to the test module); its tests are 725 lines and 807, 5 tests more. The comparison of the lines cargo reads (`directives`, from `BuildOutput::parse`) is kept as
it was.

What is new: `Run` (the lines and the digest), `current`, the lock, and the digest following links. A
link is read as its text and what it leads to (a file's bytes or a directory's tree), as `include!` and
`fs::read` follow it; a directory reached a second time is named, not read again; a link that leads
nowhere or round in a circle is its text alone. Times and permissions are not in the digest: rustc
reads neither.

### The verifier's cases

Each of the eight cases that failed in round 1 or 2, or that round 1 checked, is a test here. The
verifier's tests asserted two things: that the check said "different", and that cargo's directory
still held `old` after it. The second half was about the copy and cannot hold in place: blaze's run is
the one in cargo's directory. It becomes: the directory holds what cargo's run in it leaves (`new`).

| Case (round) | Now | Test |
|---|---|---|
| Directory older than an input, a `make` style script (1) | the script finds the real directory: `Err`, `new` | `a_directory_older_than_the_input_is_as_old_as_under_cargo` |
| Write through a link out of the directory, `../state` (1) | the write reaches `state`, as under cargo; the digest follows the link: `Err` | `a_write_through_a_link_out_of_the_directory_is_a_difference` (relative and absolute) |
| A line that is not UTF-8 (1) | unchanged code: `Err` in both comparisons | `a_line_that_is_not_text_is_not_a_line_cargo_reads_in_either_run` |
| `OUT_DIR` missing (1, passed) | `Err`; nothing to clean up | `a_directory_that_is_missing_is_an_error` |
| Script fails (1, passed) | `Err`, and no record of a run | `a_script_that_fails_is_an_error_and_leaves_no_record_of_a_run` |
| Two names of one file, `-ef` (2) | one file, as under cargo: `Err`, `new` | `two_names_of_one_file_are_one_file_as_under_cargo` |
| A link to a link, the middle one pointed elsewhere and back (2) | the real chain: `Err`, `new` | `a_link_to_a_link_leads_through_it_as_under_cargo` |
| An extended attribute (2, macOS) | the real file: `Err`, `new` | `an_extended_attribute_is_there_as_under_cargo` (macOS only) |

Round 2's two controls hold: a link the script points at another file with the same bytes is a
difference (`a_link_the_script_points_elsewhere_is_a_difference`), and old whole-second times are not
compared at all (`the_digest_of_a_directory_is_of_what_a_compiler_reads_there`). Round 2's findings on
Windows (a directory opened as a file to set its times, which sent every rerun there to cargo) and on
races while copying are gone with the copy: no directory is opened as a file or copied. A write during
the digest is still possible from outside cargo, and makes the digest another, which sends the build
to cargo. Round 2's harness finding is fixed too: `differences` compared output after
`program_output` took out what looked like cargo's lines, a missing final newline and times. The
harness now builds cargo's reference program and runs it directly, compares exit status, standard
output and standard error byte for byte, and its self-test has the verifier's three cases, which the
old comparison passed (**read**: `program_output` gives equal triples for all three, run here).

New tests besides: a check while another process holds `.cargo-lock` runs nothing, and reads cargo's
run only when cargo's rule says it is current (`a_run_while_a_cargo_builds_in_the_directory_is_cargos`;
its lock is held by a `perl` process, because a lock this test process held was also held by a process
another test started at that moment, which made it fail one run in one when all ran together); a run
is not made twice after a change (`the_last_run_since_what_the_script_reads_changed_is_not_made_again`);
and a run that differs is still a difference when read again with nothing changed
(`a_run_that_differs_is_different_until_cargo_builds_again`).

### What in place changes

- A run that differs leaves `OUT_DIR` as it left it, as cargo's run would. The build is cargo's, and
  cargo runs the script again there before it compiles the package.
- A file the script writes again with the bytes it had is the same now (it was "wrote something else"
  by its time). A script that writes the name of its own `OUT_DIR` into a file is the same as well,
  where a copy, with another name, never was (CMake's cache in `OUT_DIR` holds that name).
- A link that leads out of `OUT_DIR`, or nowhere, is checked; it sent the build to cargo before.
  What it leads to is read at each check: a link to a large directory costs reading it.
- Under plain cargo, a script cargo has not yet run again in the build when the wrapper asks (one of a
  package the program does not depend on) sends the program to rustc; it was run in a copy.
- Each cargo build that the daemon can serve, and each load, takes a digest of the `OUT_DIR` of each
  script it can run (the record above).

### Numbers (round 3)

| | Count |
|---|---|
| `cargo +1.99.0 test -p ra-jit` on `88f76038` | 213 passed, 0 failed, 1 ignored (the round 2 verifier's run, **read** from its report; not run again here) |
| the same on this branch | **218 passed, 0 failed, 1 ignored** (2 runs, debug, the same each time; the six other targets have no tests). `cargo::script::tests`: 29 run and 1 ignored on `88f76038` (**read**, the same report), 34 and 1 now |
| `cargo::script` alone | 34 passed, 1 ignored, in each of 2 runs; the run before them had the lock test fail once, when the test held the lock itself (above) |
| `harness/scriptrerun.py --selftest` | ok, 9 cases (6 before) |
| `harness/scriptrerun.py`, this branch's release build | exit 0 in 26 s and in 17 s (2 runs, the second of the commit's release build, after a change to a comment only): the three edits the daemon's, the fourth cargo's ("the build script of `app` wrote something else"), and in all four the program's exit status, standard output and standard error byte for byte those of cargo's reference program; peak 2.5 GB, no swap growth |

**Cost of a check** (n=10, one session, release, `cargo test --release -p ra-jit --bin ra-jit -- --ignored
--nocapture the_cost`, load average 3.9 to 4.7 with other units' processes). The directory is the
made-up one of "After review" (125 files, 4,530,000 bytes, random bytes) and the script does nothing:

| | min | median | max |
|---|---|---|---|
| the record a build takes (`Run::of`, at each cargo build and each load) | 11.9 ms | 15.3 ms | 79.8 ms |
| the check: the script run in the directory, and the digest after it | 16.4 ms | 20.2 ms | 217.3 ms |

The limiter was not isolated. The files are written just before the ten runs, and "After review" found
the first read of a file just written to cost 1.5 to 2.5 ms a file on this Mac, so the maxima are
probably the first runs (**guess**: the time of each run was not printed). These are not compared with
the copy's 361 ms of "After review", which was another session.

Not run here: the gate's other parts (the LLVM build, `cargo test --release` of `hir-ty`, `hir-def`,
`parser` and `syntax`, which this change does not touch), Linux, Windows, plain cargo under `ra-jit
enable` (the wrapper's side, B, is covered by the unit test of the lock and not by a harness run), and
tracedecay itself (this Mac, as above).

## Round 4: blaze's run is cargo's run (unit u8-records)

A third verifier run (GPT 6.1) failed `6c62adc3` on two findings, both reproduced against cargo
1.99 (**read** from its report and its evidence):

1. **A script that is not idempotent ran twice for one edit.** A script that counts its runs in
   what it generates printed `3 8` under `cargo ra-jit` after one edit, and `2 8` under cargo
   alone. Blaze's check ran it, found it wrote something else, and sent the build to cargo, which
   ran it again: blaze wrote none of cargo's records of a run, so cargo's own rule still found it
   out of date. The same extra run came later after a check that found the run the same: cargo's
   next build ran the script, which cargo alone would not have.
2. **The wrong lock with `build.build-dir`.** Cargo 1.99 holds `.cargo-build-lock` in the build
   directory while it runs a script; `.cargo-lock` is in the other directory, shared. The check
   looked for `.cargo-lock` above `OUT_DIR`, found none, took that for NFS and ran the script
   while cargo's own run of it was under way.

"Round 3" said blaze's run, then cargo's, is what cargo does for an edit, a touch and a build. That
holds for a script whose second run changes nothing, and not for one that counts or appends. The
root cause of both halves of the first finding is one thing: **blaze's run was not recorded as
cargo's run.** The fix records it.

### What cargo keeps of a run, and what blaze now writes

Read from cargo at `5f94df478` (cargo 1.99.0, `cargo +1.99.0 -V` names that commit), fetched
read-only and not built or run. A run of a build script is a unit of its own (`RunCustomBuild`),
and cargo runs it when its fingerprint says it is out of date. For a dirty unit cargo does, in
this order:

| Step | Cargo 1.99 source | What blaze writes, in `records.rs` |
|---|---|---|
| Empties the fingerprint's hash, so a run that does not end is run again | `src/compiler/fingerprint/mod.rs:549` (`prepare_target`) | `Unit::begin`: `.fingerprint/<package>-<hash>/run-build-script-<target>` written empty |
| Makes `OUT_DIR` if it is not there | `src/compiler/custom_build.rs:523`, `552` | `Unit::begin`: `create_dir_all` |
| Writes `invoked.timestamp`, whose time is when the run began | `custom_build.rs:598`, `crates/cargo-util/src/paths.rs:381` (`set_invocation_time`) | `Unit::begin`: the same text, the time read back from the file |
| Runs the script, stdin closed, and fails it on a `cargo::error=` line | `custom_build.rs:598-672`, `crates/cargo-util/src/process_builder.rs:346` (`exec_with_streaming`) | `current`: the same; a failed run writes nothing more |
| Writes `output` (standard output) and gives it the time the run began, then `stderr`, then `root-output` (the name of `OUT_DIR`) | `custom_build.rs:683-688`, `paths.rs:186` (`write`), `706` (`set_file_time_no_err`) | `Unit::ran`: the same, with `std::fs::write`, as `paths::write` is |
| Reads what the script printed; a line cargo does not take fails the build | `custom_build.rs:797` (`BuildOutput::parse`) | `blaze_build_script::parse`, the existing port of that function; on an error nothing more is written |
| Writes the fingerprint: the hash in hex, then the fingerprint as JSON | `fingerprint/mod.rs:552-585` (the run's `local` recomputed after it), `1953` (`write_fingerprint`) | `Unit::record`: the fingerprint cargo wrote last, with `local` recomputed as cargo does |

Every write is a plain write of the whole file, as cargo's are; none is atomic in cargo either. The
second layout of the build directory (`build/<package>/<hash>/run/stdout`) is nightly's
(`-Zbuild-dir-new-layout`); a stable cargo 1.99 cannot use it, and a run there is not made.

**The fingerprint is written exactly.** A fingerprint's hash is cargo's `util::hash_u64`: its
`Hash` (`fingerprint/mod.rs:1378`) through `rustc-stable-hash` 0.1.2's `StableSipHasher128`
(`src/util/hasher.rs`), written as the bytes of the `u64`, least significant first, in hex
(`src/util/hex.rs`). Blaze has the fields cargo serializes, in cargo's order (`mod.rs:638`), the
dependencies as cargo's 4-tuples (`mod.rs:760`), and `LocalFingerprint` with its variants in
cargo's order (`mod.rs:815`), whose derived `Hash` depends on that order. The same crate and version
are in blaze's lock file already (by `hir-ty-codegen`), with the checksum cargo 1.99's lock has.
What changes in a run's fingerprint is `local`, which cargo recomputes from what the run printed
(`local_fingerprints_deps`, `mod.rs:1916`): the `rerun-if-changed` paths, from the package's
directory where they are below it, beside `output` as named from the build directory, and each
`rerun-if-env-changed` variable with the value cargo has for it (`from_env`, `mod.rs:909`: the
`[env]` of cargo's configuration, else its environment). A run that prints neither keeps what cargo
reckoned before it (`gen_local` returns `None`, `mod.rs:579`). Everything else (`rustc`, the
dependencies' hashes, `rustflags`, `config`) is the fingerprint cargo wrote last, as cargo
computes it again unchanged.

Two checks make "exactly" a property of each run and not of the port alone:

- **A golden fingerprint.** `records::tests` reads a run's fingerprint and a program's fingerprint
  that cargo 1.99.0 wrote (`crates/ra-jit/src/cargo/records/`, from `cargo +1.99.0 build` of a
  scratch package whose script watches `build.rs` and `data/./in.txt` and two variables, one set
  and one not), writes each again byte for byte, and hashes each to what cargo wrote beside it.
  The program's fingerprint names the run's hash among its dependencies, and it is the hash blaze
  computes (**read**: the test passes).
- **Each record before a run.** Before it runs anything, blaze reads cargo's last fingerprint of
  the run, writes it again and requires the bytes cargo wrote, hashes it and requires cargo's hash,
  and requires that what cargo would reckon of the last run now is what the record has. A record
  that is not cargo 1.99's, a variable that has another value now (cargo runs the script again,
  with that value), or a record of a run that did not end (an empty hash) leaves the build to
  cargo with nothing run.

### When cargo would run the script, and the one field blaze does not reckon

Blaze runs a script only when cargo 1.99 would, by cargo's own rule, read from its records
(`check_filesystem`, `mod.rs:1246`, and `find_stale_file`, `mod.rs:2083`): `output` is not there,
the script was compiled after it, or a path it watches (a directory by everything below it,
following links, `paths::mtime_recursive`) was written after the run began. A time equal to it is
not after it, as in cargo, and what is below cargo's home in `git` and `registry` is never newer.
When cargo would not run it, the run cargo has is read. This takes the place of the daemon's own
rule (`newest` in `inputs.rs`, deleted) and of `ra-jit-checked`, the record of blaze's runs beside
the directory (deleted): blaze's run is now in `output`, as cargo's is.

One field cannot be written exactly, and blaze does not write it: **`LocalFingerprint::Precalculated`
of a script that prints no `rerun-if-changed` or `rerun-if-env-changed`.** Cargo then fingerprints
the whole package (`build_script_local_fingerprints`, `mod.rs:1813`, calling `pkg_fingerprint`,
`mod.rs:2072`): the time of the newest file in cargo's list of the package's files and its path,
as text. The list is `PathSource::list_files` (`src/sources/path.rs`), which reads git's index and
`.gitignore`, the manifest's `include` and `exclude`, and leaves out nested packages and the target
directory. Blaze has no port of it, and an approximation would make cargo run the script again or,
worse, not run it when it should. So such a script is not run in place; its build is cargo's, with
nothing run, as before this branch for a change of its package. The script of tracedecay's CLI and
the verifier's scripts print `rerun-if-changed`. The recommended way to close it is a port of
`list_files` and `fingerprint` with a differential test against cargo on real packages
(follow-up below).

### The lock

`records::Unit::lock` takes what cargo 1.99's `Layout::new` takes (`src/compiler/layout.rs:262-292`):
`.cargo-build-lock` in the profile's directory of the build directory, exclusively, and
`.cargo-lock` there shared when it is there (the default layout, where the build directory is the
target directory; cargo takes it shared for an older cargo). Neither is waited for. The file is
found from `OUT_DIR` (`<build-dir>/[<target>/]<profile>/build/<package>-<hash>/out`, three
directories up), so `build.build-dir` needs nothing else. Cargo takes `.cargo-build-lock`
exclusively in every stable build, and shared only under nightly's `-Zfine-grain-locking`;
an exclusive try fails against either. It is taken before cargo's records are read, so a cargo
that ran the script meanwhile has written them when it lets go. While a cargo holds it, the run
cargo has is read when cargo's rule says it would not run the script; otherwise the build is
cargo's. Where there is no `.cargo-build-lock` (NFS, where cargo takes none, or a directory a
cargo 1.99 did not build in), nothing is run: nothing keeps a cargo out.

**read**, live, `cargo +1.99.0 build` of a scratch package whose script waits for a file (1 run per
layout, `/tmp/u8r/live-lock.py`): with `build.build-dir = "build-cache"`, `OUT_DIR` is
`build-cache/debug/build/lockprobe-<hash>/out`; while the script waits, an exclusive try of
`build-cache/debug/.cargo-build-lock` fails, and there is no `.cargo-lock` in `build-cache/debug`.
In the default layout, `target/debug/.cargo-build-lock` is held exclusively and `.cargo-lock`
there is shared (a shared try succeeds). An exclusive try of `.cargo-build-lock` fails in both,
which is what `Unit::lock` does.

### The verifier's cases, and the tests

| Case | Before (`6c62adc3`) | Now | Test |
|---|---|---|---|
| A script that counts its runs in what it generates, one edit | `3 8`, cargo alone `2 8` | `2 8`, 2 runs | `a_script_that_counts_its_runs_runs_once_for_a_change`; `harness/scriptrerun.py`, `counter` |
| The same, two edits | `5 9`, cargo alone `3 9` | `3 9`, 3 runs | the same |
| The same, then plain cargo (`ra-jit enable`) | `5 9`, no script run | `3 9`, no script run | `harness/scriptrerun.py`, `counter` |
| A script that counts its runs beside `OUT_DIR` (the check finds it the same), two edits, then plain cargo | 4 runs, and plain cargo ran it; cargo alone 3, and not | 3 runs, no script run | `harness/scriptrerun.py`, `logger` |
| A cargo holds `.cargo-build-lock` in a build directory set apart, with no `.cargo-lock` there | the script ran | not run, `Err` | `a_run_while_a_cargo_builds_in_a_build_directory_set_apart_is_cargos` |
| No `.cargo-build-lock` at all | the script ran | not run | the same test |

The harness rows are **read** from `harness/scriptrerun.py` on this branch's release build and on a
release build of `6c62adc3` (1 run each, the store off for these two cases: a run the store kept
stands for the script's, which is another matter, below). Plain cargo here is cargo in a cargo home
in which `ra-jit enable` was run. A cargo without blaze's wrapper has units of its own for the
workspace's packages, because cargo mixes `RUSTC_WORKSPACE_WRAPPER` into their hash
(`src/compiler/build_runner/compilation_files.rs:800`): it runs their scripts in another `OUT_DIR`,
for the first time (**read**: `1 9` in the first run of this case, from a fresh `OUT_DIR`). That is
so before this branch too.

Other new tests: a run is recorded as cargo records its own (the bytes and time of `output`,
`stderr`, `root-output`, and a fingerprint cargo's rule finds current:
`a_run_is_recorded_as_cargo_records_its_own`); a run that fails, logs `cargo::error=` or prints a
line cargo does not take leaves cargo's records of a run that did not end
(`a_script_that_fails_…`, `a_script_that_logs_an_error_…`); a variable that changed is cargo's to
run the script for (`a_variable_the_script_reads_that_changed_is_cargos_to_run_it_for`); a script
that says nothing of what it reads is cargo's (`a_script_that_says_nothing_of_what_it_reads_is_cargos`);
and the three of `records::tests` (the golden fingerprint, `local`, the hex). The tests of rounds 1
to 3 run on cargo's layout now, with cargo's records, and pass; three changed what they assert:
the record of blaze's run is cargo's (was `ra-jit-checked`), a missing `OUT_DIR` is made as cargo
makes it (was an error), and a run in the second layout is not made (was made).

The guards of `cargo ra-jit` are checked in another order: a build script is run last, when no
other guard sends the build to cargo (a manifest, cargo's configuration, a variable, a file a build
script or proc macro was built from). A run is cargo's now, so the order changes no result, only
whether a script is run that cargo was going to run anyway.

### What is not the same as cargo, still

- **A run that fails runs again under cargo.** Blaze's run fails or logs an error, cargo's records
  say the run did not end, the build goes to cargo, and cargo runs the script again and reports the
  failure: two runs where cargo alone has one. Making it one means blaze failing the build itself,
  with cargo's message (follow-up).
- **A newly watched variable that cargo sets itself.** A run that starts to watch a variable is
  recorded with the value the cargo that builds next has (its configuration's `[env]`, else its
  environment). For a variable cargo sets for the script itself (`TARGET`, `OUT_DIR`, `CARGO_*`),
  the value cargo's process has is not one blaze sees in the script's environment; blaze takes it
  from its own process, which `cargo ra-jit` runs cargo from (**inferred** to be the same; not
  tested). A variable the record already has is checked against cargo's value before any run.
- **Scripts that depend on another package's script** (`links`): cargo runs a script again when a
  script it depends on ran again; blaze's check of one script does not look at the others. Not
  introduced here, and not tested.
- **The store.** `ra-jit build-script`, which cargo runs in place of a script, gives cargo a run the
  store kept for the same key and inputs instead of running the script (`Kept`). For a script that
  counts its runs that is a run less. It is older than this unit and outside it; the harness's two
  counting cases turn the store off (follow-up).
- **The baseline** (`Run::of`, the run a build had) is still read from `output` and the directory
  as they are when the build is recorded, not under the lock (the verifier's third finding, P2).

### Numbers (round 4)

| | Count |
|---|---|
| `cargo +1.99.0 test -p ra-jit` on `6c62adc3` | 218 passed, 0 failed, 1 ignored (the round 3 verifier's run, **read** from its report) |
| the same on this branch | **226 passed, 0 failed, 1 ignored** (1 run, debug; the other targets have no tests) |
| `cargo::script` and `cargo::records` | 39 and 3 passed, 1 ignored (the measurement) |
| `harness/scriptrerun.py --selftest` | ok, 9 cases |
| `harness/scriptrerun.py`, this branch's release build | exit 0 (1 run): the four steps of before as before (three the daemon's, the fourth cargo's with "wrote something else", the program as cargo's), and the two counting cases as in the table; peak 2.6 GB top MEM, no swap growth |
| the same harness, `6c62adc3`'s release build | exit 1 (1 run): the four steps pass; `counter` wrong at all three steps (`3 8`, `5 9`, `5 9`), `logger` wrong at the plain build (4 runs, a script ran) |
| the verifier's plain-cargo scenario (`plain.py`: `ra-jit enable`, plain `cargo build`, cold and four edits of `scriptrerun.py`'s project) | every step's program byte for byte cargo's reference program's; three edits the daemon's, the bundle edit through the wrapper's existing path ("wrote something else") (1 run) |

No timing here is a comparison; nothing was measured for speed.

## Commands

All in this checkout unless a path says otherwise. `H` is the heavy lock of standing order 15 (taken
per run, released by a trap), `G` is the guard (`RA_JIT_MEMORY_MAX=16G RA_JIT_OWN_BASE_MEMORY=12G
RA_JIT_SPARE=0`) under the harness watchdog.

```
git -C /Users/bytedance/dev/tracedecay worktree add --detach /Users/bytedance/dev/td-u8 d79de82273
# in td-u8: #![recursion_limit = "256"] at the top of crates/tracedecay-code-index-runtime/src/lib.rs;
#           pnpm install --frozen-lockfile --offline   (3 m 8 s);  .pnpm/crates/config.toml moved to .off
# scratch package td-u8/local-only/u8bs: Cargo.toml with build = "../../crates/tracedecay-cli/build.rs",
#   build-dependencies flate2 =1.1.10, serde_json =1.0.154, sha2 =0.11.1, edition 2024, [workspace]
# H: E1 and E2, cargo 1.99.0 only (the four builds and the two reruns of section 1)
python3 -I exp1.py
# H, G: E3, blaze build on the scratch package, cold and 3 edits, with the fork's and this branch's binaries
python3 -I e3.py /Users/bytedance/dev/blaze-targets/u1/release u8-work/e3-before 3
python3 -I e3.py /Users/bytedance/dev/blaze-targets/u8/release u8-work/e3-after 3
# H: E4, the optional patch applied to td-u8's build.rs, then E1 again, then reverted
# H: the build of this branch
CARGO_TARGET_DIR=/Users/bytedance/dev/blaze-targets/u8 cargo +1.99.0 build -p ra-jit --release
# H: the unit tests
CARGO_TARGET_DIR=/Users/bytedance/dev/blaze-targets/u8 cargo +1.99.0 test -p ra-jit
# H, G: the new harness, against the fork's binaries and this branch's
RA_JIT_BIN=.../u1/release/ra-jit python3 harness/scriptrerun.py
RA_JIT_BIN=.../u8/release/ra-jit python3 harness/scriptrerun.py
# round 3, CARGO_TARGET_DIR=/Users/bytedance/dev/blaze-targets/u8p (cp -cR of u8fix), each line under H and G
cargo +1.99.0 test -p ra-jit --bin ra-jit -- cargo::script    # 3 runs: 33 and 1 failed, then 34, 34
cargo +1.99.0 test -p ra-jit
cargo +1.99.0 build -p ra-jit --release
RA_JIT_BIN=.../u8p/release/ra-jit python3 harness/scriptrerun.py --selftest
RA_JIT_BIN=.../u8p/release/ra-jit python3 harness/scriptrerun.py
cargo +1.99.0 test --release -p ra-jit --bin ra-jit -- --ignored --nocapture the_cost
cargo +1.99.0 fmt --all -- --check; git diff --check                  # not under H
```

Round 4, `CARGO_TARGET_DIR=/Users/bytedance/dev/blaze-targets/u8r` (`cp -c -R` of `u8p`), each line under
H and G, in this checkout unless it says otherwise:

```
# cargo 1.99's sources, read-only, never built or run: src/compiler/{custom_build.rs,fingerprint/mod.rs,
#   layout.rs,build_runner/compilation_files.rs,...}, src/util/{hex.rs,hasher.rs}, crates/cargo-util/src/
#   {paths.rs,process_builder.rs} at 5f94df478, with curl into /tmp/u8-records-cargo-src
# the golden fingerprints: a scratch package (build.rs watching build.rs, data/./in.txt, U8R_GOLDEN=on, U8R_UNSET)
U8R_GOLDEN=on cargo +1.99.0 build --offline       # in u8r-scratch/golden, CARGO_TARGET_DIR=its target
cargo +1.99.0 test -p ra-jit --bin ra-jit -- cargo::script cargo::records   # 42 passed, 1 ignored
cargo +1.99.0 test -p ra-jit                                               # 226 passed, 1 ignored
cargo +1.99.0 build -p ra-jit --release
python3 harness/scriptrerun.py --selftest                                   # not under H
RA_JIT_BIN=.../u8r/release/ra-jit python3 harness/scriptrerun.py            # exit 0
# 6c62adc3 in a detached worktree, CARGO_TARGET_DIR=.../u8r-r3 (cp -c -R of u8r)
cargo +1.99.0 build -p ra-jit --release
RA_JIT_BIN=.../u8r-r3/release/ra-jit python3 harness/scriptrerun.py         # this branch's harness: exit 1
RA_JIT_BIN=.../u8r/release/ra-jit python3 /tmp/u8r/plain.py                 # the verifier's plain.py
python3 -I /tmp/u8r/live-lock.py separate; python3 -I /tmp/u8r/live-lock.py default   # under H
cargo +1.99.0 fmt --all -- --check; git diff --check                       # not under H
```

The scripts and raw outputs of E1 to E4 are in `/Users/bytedance/dev/blaze-targets/u8-work` (`exp1.py`,
`e3.py`, `e1/`, `e3-before/`, `e3-after/`, `e4/`); they are not part of the repository.
