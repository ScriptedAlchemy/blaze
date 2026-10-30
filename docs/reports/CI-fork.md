# CI on the fork

`pr.yaml` is the shared caller. It builds `bundle-linux-x64` and, for areas that
need it, `bundle-macos-arm64`. Each contains the release CLI, its compiler-owned
macro runtime, and the executables selected in `.github/ci/test-sets.json`.

Consumers use `.github/actions/bundle` with the producer's manifest digest from
the job-output channel. `bundle.json` binds the source tree, flake, lockfile,
toolchain, roots and individual file digests. The verifier rejects a mismatched
checkout or changed file before a test can use it. `bundle.py test` replays the
processes Cargo recorded, including its working directory and environment; it
never compiles a test in a consumer. `bundle-check.yaml` runs tracing/audit and
native-contract checks once per platform. The caller's `ci` includes all selected
areas, and its top-level `server` preserves the old required-check name.

The schema and producer implementation live in `.github/scripts/bundle.py`.
The central build job owns Actions-cache writes within the fork's 10 GB budget;
consumer workflows cannot create their own store, compiler or Cargo-target caches.

## Server, benchmark and Nix consumers (astra)

The old server job serialized the release build and owned macro bootstrap before replay
and the remaining tests. The consumer keeps all 23 original Cargo test selections as
recorded process replays, including package/target/features, filters and thread counts.
The metadata example is also produced centrally. Both server tests and saved-source replay
verify the bundle before execution; `server` rejects failures, cancellations and skips.

The benchmark producer compiles **both benchmark executables**, rather than a complete
base ra-jit. The consumer checks the PR base identity and equal Cargo arguments, verifies
the executable pins, and compares the pair on one runner. The existing 3-second samples,
10% threshold and retry on a slowdown are unchanged. A missing base is a failure unless
the producer explicitly records that the base lacks `crates/ra-jit-bench`.

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

## Distributed protocol consumer (fable)

`blaze-dist-review.yaml` was an entry point whose 14 `C2 runtime (os, lane)` jobs each ran the
same lane-independent setup before the lane's own differential. It is now a `workflow_call`
workflow `pr.yaml` calls once per platform with the bundle's digest and the list of lanes each
job runs: `protocol` (Linux) checks formatting, replays the `dist` set and runs `buildspec.py
--self-test`; a `C2 runtime (os, lanes)` job takes the bundle, records its identity and runs its
lanes in turn (a lane is a `buildspec.py --ci-group`, or `saved_spec.py` under one backend),
keeping every lane's log and result, and fails if any lane failed. Linux runs one lane per job;
macOS packs the seven lanes into four jobs (`basic`, `profiles-native`, `commands`, and
`workspace nested saved-cranelift saved-llvm`), which with `bundle (macos-arm64)` is the fork's
five concurrent macOS jobs. The tracing-integrity and platform-contract checks that every lane
repeated are `bundle-check.yaml`'s, once per platform. The required contexts `C2 runtime
(ubuntu-24.04)` and `C2 runtime (macos-15)` are jobs of `pr.yaml`: each requires its platform's
bundle check and every lane of its platform, or the dist area not being selected.

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

After: pending completed runs of `pr.yaml` on this branch. Negative control: pending.
