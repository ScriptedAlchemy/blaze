# CI on the fork: one build per platform, every check consumes it (2026-10-30)

What upstream's CI does is in [harness.md](../harness.md#ci) and [CI.md](CI.md). This is the
fork's (ScriptedAlchemy/blaze), whose integration branches are `dist/*` and whose pull requests
go to `dist/c4-keys`: what I measured, the design that follows from it, and the contract the
checks are written against.

## What I measured before

On PR18 (head e442c970, runs of 2026-10-30), and the fork's Actions settings:

- **Every check built its own ra-jit.** `blaze-dist-review.yaml` ran 7 C2 lanes on Linux and
  macOS, 14 jobs, and each job did the same 20 to 30 minutes before its own differential: the
  toolchain (32 to 40 s), `norustc_test.py` (63 to 77 s, most of it the first `nix develop
  .#bench`), `cargo test -p blaze-wire -p blaze-spec` (271 to 434 s), the release build of
  ra-jit (628 to 888 s, `CARGO_BUILD_JOBS=2`, no cache) and the owned macro runtime's bootstrap
  (230 to 380 s). `blaze-serve-review.yaml` built the release CLI again (about 14 minutes) and
  bootstrapped the runtime again (about 6), and compiled its test crates in debug besides;
  `bench.yaml` built its benchmark twice (148 s base, 122 s head, run 41098738519).
- **The first `nix develop .#bench` of a job took about 75 s**, all of it building the Rust
  toolchains rust-overlay assembles on the runner (stable and the nightly with its docs), which
  no substituter has. The toolchain action's own `nix develop` of the default shell, which those
  jobs never used, took another 18.6 s.
- **The registry is not worth caching**: `Updating crates.io index` and the download of all 165
  crates took 1.2 s.
- **The fork is on the free plan**: 20 concurrent jobs, 5 of them macOS. macOS jobs sat about
  20 minutes unassigned and were cancelled; one push of PR18 asked for 7 macOS jobs at once.
- **The Actions cache was full and useless across pull requests**: 11.0 GB in 36 entries, all of
  them on `refs/pull/*/merge` (rust-cache entries of `bench`, `demo` and `live-edits`, one per
  push), none on `dist/*`. A pull request's entries are read by that pull request alone, and
  upstream's workflows write their sccache and rust-cache on `main`, which the fork never
  builds, so every build was cold.

## The design

`pr.yaml` is the one entry point of a pull request into `dist/main` or `dist/c4-keys`, and of a
push to either of those two branches (a push to any other branch starts nothing: its pull
request's run is the one that counts, and a second run per commit on the `push` event doubled
the load on the fork's twenty runners):

```
changes ─▶ build linux-x64 ──▶ bundle (linux-x64) ─┐
        │                  └─▶ area checks ─────────┤
        └▶ build macos-arm64 ─▶ bundle (macos-arm64)┼─▶ ci
                           └─▶ area checks ─────────┘
```

- `changes` reads the diff against `.github/ci/areas.json` and says which areas run; macOS is
  built only when an area with macOS checks runs. A change to the CI runs everything.
- `build` (`bundle.yaml`, one job per platform) is the only place ra-jit is compiled. It builds
  the release binaries, bootstraps the owned macro runtime with them, compiles every test set
  the checks need, and uploads the bundle.
- Each area is a `workflow_call` workflow. Called workflows run inside the caller's run, so they
  download the bundle artifact directly; artifacts do not cross separate runs, which is why the
  areas stop being entry points.
- `bundle (<platform>)` (`bundle-check.yaml`) runs what depends on the platform alone, once per
  platform: the bundle's verification, the runtime's pin against this ra-jit, `norustc_test.py`
  and the C2 contract tests. Areas do not repeat them.
- `ci` says whether everything that ran passed.

## The build bundle (contract v1)

The pull request that introduced it (ScriptedAlchemy/blaze#32) carries the same contract.

**Artifact** `bundle-<platform>` (`linux-x64`, `macos-arm64`): `bundle.json` and
`bundle.tar.zst`. The build job's output `sha256` is the sha256 of `bundle.json`, passed to each
consumer as an input: the digest travels through job outputs, not through the artifact it
vouches for.

**bundle.json** names the source (`commit`, `tree` = `HEAD^{tree}`, `flake` = sha256 of
`flake.nix` and `flake.lock`, `cargo_lock`), the toolchain (`rustc -vV`, `cargo -V`,
`llvm-config --version` in `nix develop .#bench`), the features (`llvm`), the two roots
(`$GITHUB_WORKSPACE`, `$RUNNER_TEMP`), every file as `workspace/…` or `temp/…` with its sha256,
size and mode, and the sets.

**Contents and where they land:**

| what | path |
|---|---|
| release ra-jit, cranelift and llvm (`cargo build --locked --release -p ra-jit --bins --features llvm`) | `target/release/{ra-jit,cargo-ra-jit,ra-jit-rustc,ra-jit-rustc-first}` |
| the owned macro runtime (`owned_macro_server.py --blaze target/release/ra-jit --work $RUNNER_TEMP/owned-runtime`) | `$RUNNER_TEMP/owned-runtime/blaze-proc-macro-srv`, `image-pins.json` and the bootstrap's audit records |
| each set's test executables, the bins and examples they run, dynamic libraries, and the build-script output of local packages | their cargo paths under `target/` |
| the verified manifest | `$CI_BUNDLE_JSON` (never a `BLAZE_*` name: ra-jit takes every `BLAZE_<NAME>` of its environment as its setting `RA_JIT_<NAME>`, and `cargo spec` refuses one it does not know) |

**Sets** (`.github/ci/test-sets.json`, owned entry by entry by the area that runs them): each is
its own cargo command in the bench shell with `CARGO_PROFILE_DEV_DEBUG=0`,
`CARGO_PROFILE_TEST_DEBUG=0`, `CARGO_INCREMENTAL=0`, so its feature resolution is the one the
command alone has.

- `test`: `cargo test --locked <args>` with a cargo `runner` (`bundle.py _record`) that writes
  down each process cargo would start (executable, arguments, working directory and the
  environment cargo adds) and starts nothing. Doc-tests, which stable cargo runs through rustdoc
  and not through the runner, run for real in the build job. A set that records no process
  fails the build.
- `build`: `cargo build --locked <args>`; `bench`: `cargo bench --locked --no-run <args>`. Their
  executables are listed by name.
- `source: base` builds the set from the pull request's base commit (a detached worktree, a
  target directory of its own) with the same shell, toolchain and environment as the head, and
  records the base's identity. `requires: <path>` makes a set `absent`, with its reason, when the
  path does not exist in its source.

**Verifying** (`.github/actions/bundle`): the job fails unless sha256(`bundle.json`) is the
build job's output, `source.tree`, `source.flake` and `source.cargo_lock` are the checkout's,
the roots are this runner's, the tar has nothing unlisted, and every file unpacked has its
sha256 and size. `bundle.py test <set> -- <libtest args>` checks the executable's digest again
and replays the set's processes in cargo's order with their recorded directory and
environment over the job's own, stopping at the first failure with its status, as `cargo test`
does without `--no-fail-fast`. `bundle.py path <set> <name>` prints an executable's path. Neither
runs cargo or rustc.

## Cache policy

The bundle job alone writes the Rust and Nix caches. It writes them on a push to `dist/main` or
`dist/c4-keys`, which every pull request into that branch reads, and on a pull request's own ref, which that
pull request's later pushes read. After saving, it deletes the entries of the same kind,
platform and ref that it superseded (`.github/actions/toolchain/save`, `actions: write`), so a
ref holds one entry per platform and kind, not one per push.

- **sccache**, a local directory capped at 3 GB, keyed
  `sccache-v1-bundle-<OS>-<arch>-<flake hash>-<commit>` and restored by the prefix without the
  commit. sccache keys each compile by the compiler's hash, its arguments, its input files and
  the environment variables dep-info lists; crates that link (bins, proc macros) are never
  cached, so `ra-jit`'s own crate always compiles.
- **Nix**: only the paths of the bench shell that no substituter has, keyed
  `nix-shell-v1-<OS>-<arch>-bench-<hash of flake.nix, flake.lock, nix/>`, exact match only.
  Consumers restore it read-only (`cache: nix`, `cache-save: false`).
- A hit only makes the build faster. What is checked is fixed by `source.tree` in `bundle.json`,
  which every consumer verifies against its checkout.

## The areas' consumers

### Server, benchmark and Nix consumers (astra)

The old server job serialized the release build and owned macro bootstrap before replay
and the remaining tests. The consumer keeps all 23 original Cargo test selections as
recorded process replays, including package/target/features, filters and thread counts.
The metadata example is also produced centrally. Both server tests and saved-source replay
verify the bundle before execution; `server` rejects failures, cancellations and skips.

The benchmark producer compiles **both benchmark executables**, rather than a complete
base ra-jit. The consumer checks the PR base identity and equal Cargo arguments, verifies
the executable pins, and compares the pair on one runner. The existing 3-second samples,
10% threshold and retry on a slowdown are unchanged, and the retry confirms the same benchmark
(below). A missing base is a failure unless
the producer explicitly records that the base lacks `crates/ra-jit-bench`.

**What `tango` failed on, and how it now judges (2026-10-30).** PR18's `tango` failed repeatedly
on `store/has_miss` at +48%. A perf writer fetched the pair from seven runs: the `bench-base` and
`bench-head` binaries were bit-identical across all seven (and the diff of the store,
`hir-ty-codegen`, the bench crate and `Cargo.lock` between a failing and a passing head was empty),
so the code is not the variable. The one failing run whose CPU was recorded ran on an AMD EPYC 7763
(Zen 3); the six passing runs ran on EPYC 9V74, EPYC 9V45 and Xeon 6975P. That is a lead, not
proof: a layout effect of the head binary that shows on one CPU family only, or that family's own
noise under the 3 s samples. Nor was the failure one benchmark twice. In run 41131270465 the first
comparison was over the threshold on `pack/check_1000x512` (+23%, equal medians, a burst of
outliers raised the mean) and `store/has_miss` was at -1.6%; the second was over on
`store/has_miss` (+48%, a stable 143 ns against 96 ns, so a state of that process) and
`pack/check_1000x512` was at -0.2%. The job's comment said "twice in a row" and the script failed
when any benchmark was over in each. It could not be reproduced on an AArch64 Mac (three
comparisons, all within 3%).

The job now does three things (`.github/scripts/bench_verdict.py`, tested by
`test_bench_verdict.py` on the output of that run):

- **The same benchmark in both comparisons.** The threshold (10%), the 3 s samples and the number
  of comparisons (a second one only after a slowdown) are unchanged; the job fails only when a
  benchmark is over the threshold in both. A slowdown that moves to another benchmark, or does not
  come back, is "noise" and passes.
- **An A/A control first.** `bench-base` is compared with a copy of itself on the same runner. If a
  benchmark is over the threshold there, the runner cannot tell a slowdown from itself, the verdict
  is "environmental", the comparison with the base is reported with the numbers and not counted
  (a warning, exit 0), and everything is still uploaded. It adds 18 s to the job (six benchmarks
  of 3 s, once; run 41136287813). It catches a runner whose processes differ from each
  other, which is what the failure was: it does not catch one whose head-only process is slow
  while the control's two are not, so the CPU record below is how that is told.
- **The CPU beside every verdict.** `lscpu`'s model name and count, `/proc/cpuinfo`'s flags and
  both binaries' sha256 are in the step summary, at the top of `bench.txt` and in the
  `tango-<run>-<attempt>` artifact (`tango/cpu.txt`, with `aa.txt`, `compare-N.txt`,
  `verdict.txt` and the raw samples), on pass and on fail; the verdict's first lines name the CPU
  and the digests. Open: whether every failure lands on Zen 3.

The control reproduced the failure with no code involved. In the first dispatched run of the job
(41136287813, `pr.yaml` by hand, base and head of this change), on an AMD EPYC 7763 (4 CPUs),
`bench-base` against a copy of itself was `store/has_miss` +49.41% (95.2 ns against 142.3 ns, close to
PR18's 96.7 ns against 143.2 ns) and the comparison with the base had nothing over the threshold; the
verdict was "environmental". So the +48% is a state of a process on that CPU family, which the
base's own binary shows against itself, and not a property of the head's code. Why a process
lands in the slow state (its memory placement, as ASLR varies it, or the CPU) is still open.

What changed on purpose in the server checks, beside where the binaries come from (opus's
cross-review of 2026-10-30, S1 to S5):

- The empty probe (`harness/dist/empty_probe.py`) used a debug ra-jit the job built
  (`cargo build -p ra-jit --bin ra-jit`), and still does: the build set `serve-debug-cli` is
  that build, and the probe takes `bundle.py path serve-debug-cli ra-jit`. The dev profile keeps
  `debug_assertions` and overflow checks, which the release CLI drops.
- The saved-source replay used a release ra-jit built without `--features llvm`; it now uses the
  bundle's llvm-featured binary under `--backend cranelift`. The bundle is one binary with both
  backends, as the brief fixes it, so a `cfg(feature = "llvm")` difference on the cranelift path
  is now covered by every check of that binary rather than exercised by a build without it.
- The server's tests were built by rustup's 1.99.0 outside the Nix shell, against the runner's
  C toolchain and libc; the bundle builds every set in `.#bench`, so they are built by the
  flake's 1.99.0 with Nix's cc and glibc. Same compiler release, one build environment for
  everything a run checks.
- `serve-syntax`, `serve-server` and `serve-wire` are `cargo test -p <crate>` without a target
  selector, so those crates' doc-tests run for real in the build job, unfiltered (before, only
  the doc-tests matching the step's filter ran). Stronger; a doc-test failure in one of them now
  fails `build linux-x64`.
- `server-tests`, `saved-spec` and bench's `tango` call the toolchain action with `cache: nix`
  and `cache-save: false`: they restore the producer's Nix shell entry read-only instead of
  rebuilding the rust-overlay toolchains (about 75 s each on PR18), and spend no budget.
  `check_nix_workflow.py` accepts the toolchain action's `cache:` only with `cache-save`
  literally `false`, and still refuses any cache write in nix-live.

`nix-live.yaml`'s `demo` and `live-edits` jobs still build ra-jit themselves (`cargo test
--release -p ra-jit-live`, and on macOS `cargo build --release -p ra-jit`), against the rule.
They are not converted here because nix-live is a run of its own (push to `dist/*`, the nightly
schedule, by hand), not called by `pr.yaml`, and artifacts do not cross runs: converting them
means moving the two jobs into a `workflow_call` consumer that `pr.yaml` calls for the `nix`
area, with a `nix-live` test set (`--release -p ra-jit-live`) in the bundle and the macOS job
taking the bundle's release ra-jit for `harness/live_e2e.py`, while `unit-graphs`, `nix`,
`nix-darwin` and `nightly` stay in nix-live; `check_nix_workflow.py`'s expected-job table moves
with them. That is the next step for the nix area, not done in this change.

Measured baseline, fork PR18 head `e442c970` (2026-10-30):

| Work | Before (seconds) | After |
| --- | ---: | --- |
| Server release CLI | 941 | Central bundle; pending completed run |
| Server owned runtime bootstrap | 389 | Central bundle; pending completed run |
| Server job total | 4890, cancelled during replay | Pending |
| Benchmark base compilation | 148 | Central bundle; pending |
| Benchmark head compilation | 122 | Central bundle; pending |
| Benchmark comparison | 36 | Pending |
| Benchmark job total | 330 | Pending |
| Nix Linux build step / whole job | 1673 / 1752 | Pending |
| Nix unit graphs / Linux demo jobs | 56 / 58 | Pending |
| Nix workflow elapsed | 4882, cancelled | Pending |

Sources: [server run](https://github.com/ScriptedAlchemy/blaze/actions/runs/41098738293),
[benchmark run](https://github.com/ScriptedAlchemy/blaze/actions/runs/41098738519),
[Nix run](https://github.com/ScriptedAlchemy/blaze/actions/runs/41098738162).
Both macOS Nix jobs were cancelled without completed checks, so no successful baseline
runtime is claimed for them. Baseline server and Nix totals are censored, not successful
completion times. No after speedup is claimed until real runs complete.

The superseded #29 experiment supplied a real negative control: commit `acf04b89` replaced
flake evaluation with an echo; [unit-graphs failed in 7 seconds](https://github.com/ScriptedAlchemy/blaze/actions/runs/41110523622/job/124252053837)
with `required command 'nix flake check' is missing or only mentioned, not run` (exit 1).
Commit `e49779de` reverted it. #29 and its branch were retired for this shared design.
The Nix checker now also rejects Actions store/target/compiler caches in consumers; only
the bundle producer spends the fork's shared 10 GB. The optional external FlakeHub service
remains separate from that budget. Its opt-in and every existing Nix gate remain checked.

The redesigned server consumer also runs a negative control on the actual downloaded
bundle: it flips one byte in a server test executable, requires `bundle.py test` to
reject its digest before execution, restores the byte in `finally`, then runs the
unchanged test selections. The benchmark consumer runs nine provenance rejection tests
before preparing its pair. These are normal CI tests of rejection, so they do not need
a broken shared-branch push that would cancel the other lanes' measurements.

### Distributed protocol consumer (fable)

`blaze-dist-review.yaml` was an entry point whose 14 `C2 runtime (os, lane)` jobs each ran the
same lane-independent setup before the lane's own differential. It is now a `workflow_call`
workflow `pr.yaml` calls per platform with the bundle's digest and the list of groups each job
runs: `protocol` (Linux) checks formatting, replays the `dist` set and runs `buildspec.py
--self-test`; a `C2 runtime (os, groups)` job takes the bundle, records its identity and runs
its groups in turn (a group is a `buildspec.py --ci-group`, or `saved_spec.py` under one
backend), keeping every group's log and result, and fails if any failed. The tracing-integrity
and platform-contract checks that every lane repeated are `bundle-check.yaml`'s, once per
platform.

**Tiers (owner decision, 2026-10-30).** The C2 groups run in two tiers, modelled on
ra-jit.yaml's:

- The **fast tier**, a pull request's (and `workflow_dispatch -f tier=fast` on a branch without
  one), runs when the diff touches the dist area's paths, on Linux alone, all
  seven groups packed into three jobs: `basic` | `commands nested` | `profiles-native workspace
  saved-cranelift saved-llvm`. One job for all seven would run 4 to 5 hours (the measured
  Linux group times below add up to about 15,600 s, with `basic` and `profiles-native` censored
  at their failures); the longest group, `basic`, took 93 min alone, so the fast tier's wall
  clock is at least that, and three is the fewest jobs where no job is longer than it:
  `commands` + `nested` is 65 + 19 min, and the other four about 45 + 15 + 19 + 18 min.
  No macOS bundle is built for the dist area on a pull request.
- The **full tier** runs nightly (`schedule`), by hand (`workflow_dispatch` with `tier=full`,
  the default) and on a push to `dist/main` or `dist/c4-keys`: every area, and every C2 group on
  Linux (one per job) and macOS (packed into four jobs, `basic`, `profiles-native`, `commands`,
  `workspace nested saved-cranelift saved-llvm`, which with `bundle (macos-arm64)` is the
  fork's five concurrent macOS jobs). A failed full tier opens the issue "Full tier of pr.yaml
  failed", or comments on it while it is open (`.github/actions/nightly-issue`, which gained a
  `title` input), except when started by hand, where the person who started it is watching.
  The schedule fires from the repository's default branch only, so the nightly begins once
  `pr.yaml` is on it; until then the full tier is the integration pushes' and the dispatched runs'.

macOS C2 coverage is therefore nightly and on push, not per pull request: an owner-accepted
trade-off for the fork's five macOS runners, where the seven macOS lanes of one pull request
waited up to 90 min for a runner and one push asked for seven at once.

**Required-check names.** `C2 runtime (ubuntu-24.04)` and `C2 runtime (macos-15)` are gone: a
pull request no longer reports macOS, so a required macOS context would never be set. The one
stable context is `C2 runtime`, a job of `pr.yaml`: it passes when the tier's groups passed on
each of its platforms with that platform's bundle check, or the dist area was not selected.
Branch rules that named the two old contexts need `C2 runtime` instead; `ci` still covers
everything that ran.

Measured baseline, fork PR18 (runs [41035852934](https://github.com/ScriptedAlchemy/blaze/actions/runs/41035852934)
and [41098738023](https://github.com/ScriptedAlchemy/blaze/actions/runs/41098738023), 2026-10-30),
seconds per lane job before its own differential:

| Setup repeated per lane job | ubuntu-24.04 | macos-15 |
| --- | ---: | ---: |
| toolchain | 32-46 | 98-144 |
| tracing integrity (`norustc_test.py`) | 60-95 | 136-255 |
| platform contracts (`cargo test -p blaze-wire -p blaze-spec`) | 271-645 | 2038-2701 |
| release build of ra-jit, both backends | 628-1060 | 986-1478 |
| owned macro runtime bootstrap | 230-426 | 328-403 |
| total per lane job | 1221-2272 | 3586-4981 |

The lanes themselves (ubuntu-24.04, completed): workspace 869, nested 1131, commands 3877;
saved-cranelift 1151 and saved-llvm 1058 to a failure; basic 5573 to a failure. Whole workflow on
run 41035852934: 3 h 13 min from start to the aggregates, with the macOS lane jobs waiting up to
90 min for a runner (the fork runs five macOS jobs at once). Total runner time of the 14 lane
jobs there: about 18 h.

What changed on purpose: the lane jobs checked out the pull request's head
(`github.event.pull_request.head.sha`); as bundle consumers they check out the run's default
ref, the pull request merged into its base, because the bundle is built from that tree and the
bundle action refuses any other. The C2 lanes therefore test what would be merged, as `protocol`
already did, and a lane can fail on a change the base made that the head never had; that is the
intended subject, since it is the commit the merge would produce.

After: pending completed runs of `pr.yaml` on this branch. Negative control: pending.

## Measured after

(filled in from the runs on ScriptedAlchemy/blaze#32)
