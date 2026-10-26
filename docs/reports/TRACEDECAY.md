# blaze on tracedecay, measured against cargo

Unit u1-measure of the distributed compile, cache and tests program, started by u1 and finished by
u1b. The question: can blaze build, rebuild and test tracedecay's Rust workspace on this Mac today,
and how does that compare with cargo on the same machine?

u1 ran the measurements below on one machine, in one session (21:32 to 23:26 on 25 October 2026).
In its last cold build blaze's daemon grew a worker to 56 GB on this 48 GiB Mac and the
coordinator killed the run. u1b wrote up u1's data and added the one thing that was missing: a cold
build under a memory guard (section 5). **read** means a number was read from a run, with the run
count and spread given. **reported** means someone else saw it and I did not re-run it. Nothing is
extrapolated; what was not run is marked **unverified** or **not measured**.

## The short version

**No-go for blaze on tracedecay on this Mac today.** Four things stop it (item 7 ranks seven gaps):

- **Memory.** The daemon's worker for tracedecay grows without limit while it loads. Unguarded
  (u1, 3 runs) it held 22.0 GB at 7 s of loading, 42.7 GB a minute later and 58.3 GB when ready, on
  a 48 GiB machine. Guarded with `RA_JIT_MEMORY_MAX=16G` (2 runs) it sat at 3.4 to 3.7 GB while the
  workspace loaded, then went from 3.7 to 24.0 GB (run 1) and to 25.0 GB (run 2) in 54 s and 70 s,
  passing 16 GB with the daemon doing nothing, until the harness watchdog killed it. blaze's
  documented bound never applied to it: it leaves out a worker that is loading and the one asked
  last (item 7, gap 1). The daemon's own `blaze status` printed 16,776 MB beside "workers hold at most
  16384 MB" and let it go on.
- **Edits go to cargo.** 18 of 18 builds after an edit were cargo's, because the CLI's build
  script prints something else the second time. They took 24 to 133 s, against cargo's 6.9 to
  17.2 s (n=5 per edit, medians 3.4 to 11.5 times slower).
- **The path that never runs rustc fails on macOS arm64.** `BLAZE_WHOLE_BUILD=1` stops on
  tracedecay after 697 s, and on a hello-world project after 8.8 s, with `writable static ...
  without a rustc symbol`. The same error stops blaze's own compile of 107 build scripts in every
  build.
- **rustc 1.99.0 needs one line of tracedecay changed.** `queries overflow the depth limit!` in
  `tracedecay-code-index-runtime`; `#![recursion_limit = "256"]` fixes it.

What works: the daemon answers a build with nothing changed in 0.20 to 0.58 s against cargo's
3.13 to 8.86 s (n=3 each), and `blaze test -p tracedecay-framing` passes 13 of 13 in 0.28 to
0.33 s against cargo's 0.65 s (blaze n=2 warm, cargo n=1 warm; u1b's rerun: 0.32 s on a quiet
machine, 0.48 s with another build running). The no-op builds came from a daemon
that had loaded the 58.3 GB worker, so they cost what item 5 says; the test runs from one holding
2.6 to 5.2 GB (RSS).

On Linux: **unverified**; nothing here ran there. The docs say the whole build works there on small
projects ([self-contained.md](../self-contained.md)). What would have to be true there is at the end
of item 7.

## What was measured

| | |
|---|---|
| Machine | Apple M5 Pro, 18 cores, 48 GiB (51,539,607,552 bytes), macOS 26.6.2, 131 GB free at the guarded run |
| blaze | this fork at `c8e5f6a8`, `cargo +1.99.0 build -p ra-jit --release`, no `--features llvm`: 2 m 22 s (**read**, 1 run, u1), binaries in `/Users/bytedance/dev/blaze-targets/u1/release`. `dist/main` has since gained docs only, so the harness at `6399bf68` and these binaries run the same code |
| tracedecay | `d79de82273`, a detached `git worktree` of the owner's checkout at its HEAD, at `/Users/bytedance/dev/td-blaze-measure`; the owner's uncommitted edits are not in it |
| cargo | `cargo 1.101.0-nightly (3960fa69b 2026-10-12)` with `rustc 1.101.0-nightly (8006091eb 2026-10-17)`, from tracedecay's `rust-toolchain.toml` (`nightly-2026-10-18`) |
| blaze's compiler | Rust 1.99.0 from rustup, with `rust-src` |
| Profile | `dev`, tracedecay's own: `opt-level = 1`, `incremental = true`, line-tables-only debug info |
| What is built | `-p tracedecay-cli`, the package whose `[[bin]] name = "tracedecay"` is the shipped CLI. It depends on the library `tracedecay` (the composition crate, one 12 GB rustc process) and `tracedecay-mcp`. 572 crates compile |
| Tools compared | `cargo` (the pinned nightly), `cargo199` (cargo with 1.99.0, blaze's compiler), `blaze` (`blaze build`), `blaze-whole` (`BLAZE_WHOLE_BUILD=1`) |
| Every run | `RUST_BACKTRACE=0`, `--locked`, its own target directory, its own blaze daemon directory and store |
| Guard (u1b's runs only) | `RA_JIT_MEMORY_MAX=16G`, `RA_JIT_OWN_BASE_MEMORY=12G`, `RA_JIT_SPARE=0`, and the harness watchdog (below). u1's runs had none of these: the daemon's ceiling was the default 24,576 MB |

Everything is driven by [`harness/tracedecay.py`](../../harness/tracedecay.py), which records
one line of JSON per run. `harness/tracedecay.py --help` says how. Sizes are in MB and GB of 1,024
MB, as `top`, `footprint` and `blaze status` count them; the Mac has 48 GB of memory in that sense.

### The watchdog

Every blaze command the harness starts runs under a watchdog thread. It cannot be turned off for
a blaze tool. Every 2 s it sums the physical footprint (macOS `footprint`, which counts memory
that is compressed or swapped out; RSS leaves that out and it was most of the 56 GB) of every
process whose executable is in blaze's binary directory, plus what the daemon and workers started.
It does not count the cargo and rustc that a `blaze build` client runs, or another checkout's
blaze. It kills all of them, and the client's tree, when:

- the sum passes 24 GB (`--watchdog-mb`), or
- swap has grown by 1 GB or more since the watchdog started and less than 3 GB of it is free
  (`--swap-free-mb`), or swap has grown by 8 GB.

The kill is written to `results.jsonl` as `"outcome": "watchdog-killed"` with the reason, not as an
error. The swap rule differs from "free under 3 GB" on purpose: macOS grows its swap files on
demand, and `free` was 0.9 of 8.0 GB before any run started, from the previous run's pages, so
the plain rule would have stopped every run at once. The watchdog is shown killing a run in the
commands section.

### What had to be changed, and why

1. **One line in tracedecay**, in a throwaway worktree: `#![recursion_limit = "256"]` at the top
   of `crates/tracedecay-code-index-runtime/src/lib.rs`. Two other crates of the workspace carry
   it. Without it blaze, and any cargo using 1.99.0, fail (item 1). It was applied for every tool,
   including the cargo baseline, so all of them build the same source.
2. **`RUSTUP_TOOLCHAIN` is not set for cargo**, which builds with the nightly the project pins.
   blaze names 1.99.0 itself. `cargo199` sets `RUSTUP_TOOLCHAIN=1.99.0-aarch64-apple-darwin`.
   No `RUSTC_BOOTSTRAP` was needed: asupersync's `nightly-outcome-try` probe falls back with a
   warning on stable.
3. **A private `CARGO_HOME`**, a copy-on-write clone of `~/.cargo/registry` and `~/.cargo/git`.
   Another session on this Mac runs tracedecay builds through a cargo that holds the package
   cache lock for minutes; u1's first blaze run sat on `Blocking waiting for file lock on package
   cache`.
4. **`.pnpm/crates/config.toml` moved aside** in the worktree. `pnpm install` (needed: the CLI's
   build script embeds the dashboard and runs `pnpm run build`) writes it, and it replaces every
   crates.io and git source with a directory of symlinks into pnpm's store. Cargo then hashes
   every file of the crates it builds each time it starts. With Microsoft Defender opening the
   files that cost one to four minutes before any compile started: u1's first, unrepresentative
   cargo run took 560.85 s (1 run, **read**, contended; kept as a data point only). The moved-aside
   file is `.pnpm/crates/config.toml.off`.
5. **Target directories live in `target/`**, tracedecay's one gitignored directory
   (`target/cargo.noindex`, `target/blaze.noindex`, ...). This matters for blaze: the CLI's build
   script watches every file git does not ignore (item 7, gap 3). u1's first blaze run put them
   next to `target/`.
6. **`.noindex` directory names**, so that Spotlight (`mds_stores` was using 1.1 cores) does not
   index the build output.

### The machine was not quiet

Microsoft Defender (`wdavdaemon`, about 1 to 3 cores), tracedecay's own daemon (about 0.8 to 1.9
cores and 3 to 12 GB), `mds_stores` and CorpLink were running throughout; the harness reports them
as `noise_cores`, 1.8 to 6.7 cores on average per run. Another session on this Mac also ran
tracedecay builds through a Bazel wrapper and a released blaze in
`~/.t3/worktrees/tracedecay/bench-blaze-pr`. The harness samples other builds' CPU once a second
(`foreign_build_cores`). A run in which that peaked above 1 core is marked **contended** below. The
cargo cold and edit numbers had no contention (peak 0.09 cores or less).

## 1. Does blaze build tracedecay's CLI?

Not as it is, and with one line changed only in the mode where cargo does the build.

| Mode | Result | Label |
|---|---|---|
| `blaze build`, tracedecay as it is | fails at `tracedecay-code-index-runtime`, exit 101 after 591 s (contended: other builds peaked at 11.7 cores) | **reported**: u1's results record the exit and the line `could not compile tracedecay-code-index-runtime (lib) due to 1 previous error`; the log with `queries overflow the depth limit!` was overwritten by the next run |
| `blaze build`, with `#![recursion_limit = "256"]` | builds: exit 0 after 534.6 s (run A) and after 1094.3 s (run B); run C was killed while waiting for the daemon | **read**, 3 runs, details in item 2 |
| `BLAZE_WHOLE_BUILD=1` (no cargo build) | fails after 697 s: `unsupported: ... writable static GLOBAL_PANIC_COUNT without a rustc symbol` | **read**, 1 run |
| `BLAZE_WHOLE_BUILD=1`, hello-world (`fn main() { println!("hello"); }`) | fails after 8.8 s with `writable static CLEANUP without a rustc symbol`; the worker exits 1 | error **read** in the worker's log; the 8.8 s is **reported** (not in `results.jsonl`) |

Why the first fails: blaze compiles with Rust 1.99.0 whatever the project pins, and tracedecay pins
`nightly-2026-10-18`. cargo with 1.99.0 (`cargo199`) hits the same limit. Two other crates of the
workspace already carry `#![recursion_limit = "256"]`; the third needs it under 1.99.0 (**read**:
the patched tree builds under `cargo199`, 1 run, 266.1 s, contended, and under every blaze run).

How the second works: `blaze build` runs cargo with blaze as the rustc wrapper (`ra-jit
rustc-wrapper`), so cargo and the real rustc do the compile, and the log ends `Finished ... [optimized
+ debuginfo]` and `Waiting for blaze to load the workspace`. A daemon worker then loads the
workspace, and builds after it are the daemon's (`Finished ... [ra-jit]`) or, when the daemon cannot
build them, cargo's again. Every build, cold or after an edit, also tries to compile the 107 build
scripts of the dependency graph itself and fails each with `writable static ... without a rustc
symbol` (102 `CLEANUP`, 2 `OUTPUT_CAPTURE_USED`, 2 `GLOBAL_PANIC_COUNT`, 1 `INSTANCE`; **read**,
count per cold log and per edit log: 107 each, 20 logs). This is the macOS arm64 failure of item 7,
gap 4, not a failure of tracedecay's code.

So blaze, today, is not a way to build tracedecay without cargo. The "builds" above are cargo's
builds, with blaze's wrapper added.

## 2. Cold build

From an empty target directory, `-p tracedecay-cli`, dev profile, 18 cores. Wall seconds. "Cargo part"
is the time to cargo's `Finished` line, which blaze prints before the daemon has loaded the workspace.

| Tool | Runs | Wall s, median [min to max] | Notes |
|---|---|---|---|
| cargo (pinned nightly) | 3 | 237.9 [231.8 to 238.6] | no contention (peak other-build load 0.09 cores). Cargo's own line: 3 m 51 s to 3 m 58 s |
| cargo199 (1.99.0) | 1 | 266.1 | contended (other builds peaked at 9.5 cores) |
| blaze, run A (u1, before the guard) | 1 | 534.6, exit 0 | the client returned when cargo finished (`8m 39s`); the daemon's spare worker loaded beside cargo from the start. No contention (0.13 cores) |
| blaze, run B (u1, before the guard) | 1 | 1094.3, exit 0 | `Finished ... [ra-jit]`: the client waited for the daemon to load the workspace and the daemon answered. Contended (other builds peaked at 4.2 cores, mean 0.16) |
| blaze, run C (u1, before the guard) | 1 | killed at 767.0 | cargo part `7m 00s` (420 s), then 5.8 min of `Waiting for blaze to load the workspace`; killed by the coordinator at 56 GB (item 5) |
| blaze, guarded run 1 (u1b) | 1 | killed at 556.7 by the watchdog | cargo part `6m 55s` (415 s), then 2 min 6 s of load; watchdog at 24.0 GB. Contended: other builds peaked at 6.6 cores (mean 0.19) |
| blaze, guarded run 2 (u1b) | 1 | killed at 868.3 by the watchdog | cargo part `10m 27s` (627 s), then 3 min 41 s of load; watchdog at 25.0 GB. Contended: other builds peaked at 16.3 cores (mean 1.45), load average 27 |
| blaze-whole | 1 | fails at 697.0 | item 1 |

What the numbers say, and what they do not:

- **The cargo part with blaze's wrapper is 415 to 627 s (n=4: 415, 420, 519, 627), against cargo's 232
  to 239 s (n=3): 1.7 to 2.6 times as long.** Only A (519 s, 2.2 times) had no contention; C, guarded
  run 1 and guarded run 2 had other builds running (peaks of 5.4, 6.6 and 16.3 cores, means of 0.25,
  0.19 and 1.45), so their times are upper bounds on what the wrapper costs. The spare worker does
  not explain the spread: C (spare loading beside cargo, the default) took 420 s and guarded run 1
  (`RA_JIT_SPARE=0`, no load until cargo was done) 415 s. The limiter is not measured. Two
  candidates are in every log: the wrapper in front of each of 572 rustc calls, and the 107 build
  scripts the daemon tries to compile and fails on (item 1). **Unverified** which.
- **blaze does not build faster than cargo from cold in any run**: no run finished before cargo's
  slowest (238.6 s). The only run that got as far as an answer from the daemon (B) took 1094 s, 4.6
  times cargo's median.
- **The load after cargo is the longest step**: 5.8 min and still going at the kill (C), 2 min 6 s to
  24.0 GB (guarded run 1), 3 min 41 s to 25.0 GB (guarded run 2, contended), and B's whole wait (about
  10 min). It is where the memory goes (item 5).


## 3. What `cargo blaze status` says

`blaze status` reports builds by the daemon and by cargo, per project, and why the daemon
declined. u1 took it at four points (all **read**, 1 snapshot each):

| When | Counts | Worker |
|---|---|---|
| run A, as cargo's part ended | 0 builds by the daemon, 1 by cargo; `1 x the build script of tracedecay-cli printed something else` | loading for 7 s, holds 22,501 MB |
| run A, 61 s later | same | loading for 68 s, holds 43,759 MB |
| run B, as the build ended | 1 build by the daemon, 0 by cargo | ready, holds 59,648 MB |
| after the 5 `tracedecay-body` edits and the revert | 5 builds by the daemon, 6 by cargo; `6 x the build script ... printed something else` | a new worker, loading for 80 ms, holds 1,078 MB |
| after all 18 edit builds | 5 builds by the daemon, 20 by cargo; `20 x the build script ... printed something else` | a new worker, loading for 191 ms, holds 2,416 MB |

u1b polled `blaze status` about every 10 s through guarded run 2 (80 snapshots, **read**). With
`RA_JIT_SPARE=0` no snapshot has a builds line for the project: the daemon had no worker until
cargo finished, and the one that loaded was killed before it answered a build. So for a guarded cold
build the counts of builds by the daemon and by cargo are **not measured**; the status only says
`workers hold at most 16384 MB` and, from `loading for 10s` on, the worker's growing footprint
(item 5).

What blaze compiled and what went to cargo, as far as these counts and logs say:

- **Edits: 0 of 18 compiled by blaze, 18 of 18 sent to cargo.** The daemon's 5 builds were the cold
  build (run B), the 3 builds with nothing changed and one settle build; cargo's 20 are the 18 timed
  edit builds and 2 settle builds. That split is my reading of the counts; `status` does not name the
  builds.
- **Functions: all of them, ahead of any edit.** The worker's log in run C reads `compiled ahead:
  77345 from 77345 functions in 40.7s` (**read**; earlier lines of the same log: 2,152 of 36,436, 9,525
  of 37,510, 20,967 of 39,336 and 6,557 of 76,311). So blaze had compiled every function of the
  workspace, 1,019,121 symbols and 19,990 expanded procedural macros (`proc macros: 19990 expanded in
  22.34s`), and still sent every edit to cargo. Whether those functions would have served the edits is
  **not measured**: the line about the build script is the first in every edit log (item 4), so
  the daemon was ruled out before any edit was compiled.
- **Each edit built by cargo started another load.** After every edit the status shows a new worker
  loading, and the edit runs held 5.1 to 24.5 GB (RSS) in blaze's processes (item 5). Whether it is
  the same workspace loaded again from the store is **not measured**.


## 4. Rebuilding after an edit

**All 18 builds after an edit were cargo's.** (15 timed edits and 3 reverts of the edit, each
built once.) For each of them the log has the same lines, in this order (**read**, all 18 logs and the
settle build's):

1. `ra-jit: bug: the build script of tracedecay-cli printed something else; building with cargo,
   which ra-jit is never to do (no-rustc M, #93, and #84)`.
2. 107 lines `ra-jit did not build it ... writable static ... without a rustc symbol; rustc compiles
   the build script of <crate>`.
3. cargo's own build of the same 2 or 3 crates that plain cargo rebuilds for this edit.

`cargo blaze status` after the last edit: `5 builds by the daemon, 20 by cargo`, and `20 x the
build script of tracedecay-cli printed something else` (**read**).

| Edit | blaze, n=5: median [min to max] | cargo, n=5: median [min to max] | Median ratio | Runs contended |
|---|---|---|---|---|
| `tracedecay-body`: a number in `daemon_project_status`, crate `tracedecay` | 52.6 s [24.4 to 71.4] | 9.0 s [8.6 to 9.1] | 5.9 | 3 of 5 (other builds peaked at 1.4 to 3.5 cores) |
| `mcp-body`: the cap in `bounded_failure_reason`, crate `tracedecay-mcp` | 105.6 s [92.4 to 132.6] | 9.2 s [8.8 to 17.2] | 11.5 | 0 |
| `signature`: a parameter added to `database_recovery_guidance`, its type changed on each run | 24.5 s [24.2 to 50.8] | 7.1 s [6.9 to 7.8] | 3.4 | 0 |

The three reverts, n=1 each: blaze 24.7, 37.8 and 25.9 s, cargo 9.2, 8.9 and 7.0 s.

The limiter is not a compile: blaze's cargo ran the same crates (`Compiling` lines: 2, 3 and 2)
and `Finished` after 14.5 to 113 s against plain cargo's whole run of 6.9 to 17.2 s. How much of the
difference is the 107 failed compiles before cargo starts, how much the wrapper, and how much the
daemon's workers using the machine meanwhile (blaze's processes held 5.1 to 24.5 GB, RSS) is **not
measured**.

Why the script "printed something else": **unverified**. The CLI's build script prints one
`cargo::rerun-if-changed` per file in `source_provenance::watch_paths(&repository_root)`
(`crates/tracedecay-cli/build.rs:417-422`) when the commit is verified from git, and that list is
"every existing worktree input" (the comment at `:417-419`), so it depends on the repository's state, which
an edit or a revert changes. blaze reads a script whose second run prints something else as one it
cannot trust. The same condition was hit once in run A's cold build (`0 builds by the daemon, 1 by
cargo`) and not in run B's (`1 builds by the daemon, 0 by cargo`), on the same tree, so the cause is
not only the edit: not explained.

What does work after the daemon has loaded: a build with nothing changed (`noop`) is the daemon's,
`Finished ... [ra-jit]`: 0.58, 0.21 and 0.20 s (n=3; median 0.21 s) against cargo's 8.86, 3.13 and
3.28 s (median 3.28 s; the first of each is the one after a cold build). That daemon held the
58.3 GB worker of item 5.


## 5. Memory and disk

### The 56 GB event, and what the guard did to it

u1's runs had no guard: the daemon's ceiling was its default, 24,576 MB, as `blaze status` printed
beside every worker. Items 3 and 7 have the readings. In short: three loads of the workspace, one
(run C) with `top` showing MEM 56G of which 48G compressed, the Mac's swap going from 4.8 to 25.1 GB
(harness sample; the coordinator saw 18.7 of 19.5 GB at the kill), and the coordinator ending the run.

**Guarded runs** (`RA_JIT_MEMORY_MAX=16G`, `RA_JIT_OWN_BASE_MEMORY=12G`, `RA_JIT_SPARE=0`, watchdog at
24 GB). The worker starts only when cargo has finished, so the load is the only thing in the daemon's
processes. Footprint of blaze's processes, from the 5 s trace beside the run (**read**):

| After the worker's first sample | Run 1 | Run 2 (contended) |
|---|---|---|
| first sample | 2,880 MB (2.8 GB) | 3,190 MB (3.1 GB) |
| flat while the workspace loads | 3.4 to 3.7 GB for 72 s (`load workspace: 79.9s`) | 3.4 to 3.7 GB for 151 s (`load workspace: 159.2s`) |
| growth begins | +78 s, 5,088 MB | +157 s, 6,127 MB |
| reaches the 16 GB `RA_JIT_MEMORY_MAX` | between +106 s (14,366 MB) and +112 s (17,438 MB) | between +191 s (14,364 MB) and +199 s (16,412 MB); `blaze status` at +203 s: 16,776 MB |
| watchdog kills, over 24,576 MB | +126 s, 24,606 MB | +221 s, 25,628 MB |

From the last flat sample to the kill the footprint grew by 20.4 GB in 54 s (run 1, 0.38 GB/s) and
21.3 GB in 70 s (run 2, 0.30 GB/s). Both logs show the same order. In each, the growth begins when
`load workspace` ends, so it happens in the phases after it (**inferred** from the two timelines, not
from a profile):

| Worker log line | Run 1 | Run 2 |
|---|---|---|
| `symbols: 1019121` | in 9.6 s | in 14.5 s |
| `load workspace` | 79.9 s | 159.2 s |
| `resolve: 586 crates` | in 20.4 s | in 26.4 s |
| `proc macros: 19987 expanded` | in 19.85 s | in 23.73 s |

Both were killed after the `proc macros` line. The daemon's own `blaze status`, which run 2 polled about
every 10 s (80 snapshots), printed `workers hold at most 16384 MB` over all of them, and for the worker
`holds 12967 MB` at 190 s of loading, `16776 MB` at 203 s, `18923 MB` at 211 s and `22685 MB` at 220 s.
The daemon knew, and did nothing.

**Did `RA_JIT_MEMORY_MAX` bound it? No.** The footprint passed 16 GB and the daemon did nothing: its log
has two lines (`daemon ... listens`, `worker ... for ...`) and none of the "stopped: the workers hold N
MB, over the N MB of RA_JIT_MEMORY_MAX" that `fit()` prints (`daemon.rs:1393-1399`). **Did
`RA_JIT_OWN_BASE_MEMORY` bound it? No**, and it could not: it applies while the daemon builds its own
base, and the worker's log has none of its "not compiling the program for edits to patch any more"
lines (`main.rs:9041`). Item 7, gap 1, is the account. Only the watchdog ended it, at 24.0 GB (run 1)
and 25.0 GB (run 2), 8 and 9 GB above the ceiling the daemon was given. The guard is therefore a
bound on **the harness**, not on blaze: what the load does after 24 GB is **not measured**, and 24.0
and 25.0 GB are lower bounds on what it needs (run B: 58.3 GB).

The transient before that, in the first seconds of the cold build, is also counted by the watchdog:
13 to 17 `ra-jit` processes compiling build scripts held about 1.04 GB of footprint each (16.2 GB at the
peak of run 2's 2 s series; **read**). It went away within seconds and is not the bound's concern, but it
is why the watchdog's limit is 24 GB and not 16.

### Process memory in the other runs

Largest single process and the whole tree at the one-second sample, RSS (it leaves out memory that is
swapped out or compressed, so it undercounts blaze's by up to 2.4 times in run B):

| Run | Largest process | Tree, peak | blaze's processes, peak | Runs |
|---|---|---|---|---|
| cargo cold | 12.2 to 12.5 GB (rustc, the `tracedecay` library) | 14.6 to 15.3 GB | 0 | 3 |
| blaze edit builds (cargo's, the daemon loading again) | 3.9 to 4.0 GB | 3.0 to 4.6 GB | 5.1 to 24.5 GB, median 17.5 GB | 18 |
| blaze, nothing changed | 28 MB | 1 to 4 MB | 4.9 GB | 3 |
| `blaze test -p tracedecay-framing` (u1) | 11.6 GB (cargo building the tests) | 11.4 GB | 5.2 GB, then 2.6 GB | 3 |

Swap, from the harness: 20.4 GB used right after run B (u1's draft: from about 4.8 GB before it, not in
the record). The first edit run began at 20.3 GB and reached 23.2 GB; the other 17 began at 4.8 to
6.1 GB.

### Disk

| | Target directory | Store |
|---|---|---|
| cargo cold | 14.2 GB (14,490 to 14,526 MB, n=3) | none |
| blaze cold | 23.0 to 27.3 GB (23,594; 23,637; 23,799; 27,942 MB, n=4; 23,542 MB in guarded run 2) | 254 to 295 MB while loading, 1,202 and 1,211 MB after a full load |
| blaze whole (failed) | 849 MB | 1,247 MB |

The extra 8.9 to 13.1 GB in blaze's target directory is **not explained**; cargo's own build is the same
cargo build.


## 6. Tests

`test -p tracedecay-framing` (13 tests, the framing crate has few dependencies):

| Run | cargo (nightly) | blaze |
|---|---|---|
| 1, the test profile is built | 88.5 s | 144.2 s (cargo builds, the daemon loads); contended: no |
| 2 | 3.57 s | 0.33 s |
| 3 | 0.65 s | 0.28 s |

All 13 pass in every run (**read**, n=3 each). The tests' own time is 0.06 to 0.07 s under cargo and
0.22 to 0.24 s under blaze (`finished in`). blaze's wall is 0.28 to 0.33 s (n=2) and cargo's 0.65 s
(n=1; 3.57 s for the run before it): about 2 times faster, with n too small to give a spread for
cargo's. The daemon held 5.2 GB at the first run and 2.6 GB after (RSS, **read**).

**Rerun by u1b, under the guard** (`blaze test -p tracedecay-framing`, n=3, on the full target directory
that guarded run 2 left; cargo's test build and the daemon's load both happen in run 1):

| Run | Wall | Other builds (peak cores, mean) | Footprint of blaze's processes, peak | Verdict |
|---|---|---|---|---|
| 1 | 596.9 s | 15.8, 2.4; load average 29.5 | 3,105 MB | contended; not comparable to u1's 144.2 s |
| 2 | 0.48 s | 3.3, 3.3; noise cores 6.6 | 3,107 MB | contended; 45% above u1's top of 0.33 s |
| 3 | 0.32 s | 0.08, 0.08 | 3,103 MB | **within u1's warm spread of 0.28 to 0.33 s** |

All 13 tests pass in each run; their own time was 0.23 to 0.24 s, as u1's 0.22 to 0.24 s. So the
cheap timing reproduces when the machine is quiet (n=1 uncontended against u1's n=2), and moves with
other builds' load when it is not. The worker for this crate held 3.0 GB (footprint) once loaded,
against the 58.3 GB of the CLI's workspace: the load follows the closure of the package asked for.
The daemon was stopped after the runs (`stop`; no blaze process left).



## 7. Bugs and gaps in blaze, most blocking first

Ranked by how much each one stops tracedecay from using blaze on this Mac. None is fixed here;
this unit changes no blaze source.

**1. The memory bound exempts exactly the worker that grew.** The documentation says the workers
together hold at most half of the machine's memory, or `RA_JIT_MEMORY_MAX`, and a worker that builds
its own base at most a quarter, or `RA_JIT_OWN_BASE_MEMORY` ([cargo.md:351-358](../cargo.md),
[:418-420](../cargo.md)); it also says "a worker that is answering or loading is never stopped, nor is
the one asked last". With one workspace, the one worker is both. The code says the same:

- `over()` leaves out a worker that is `busy` and the one asked last (`daemon.rs:94-104`);
- `busy` is "answering a request, or loading a checkout" (`daemon.rs:748-753`);
- the own-base bound runs only while the daemon builds its own base (`main.rs:9030-9050`), and a
  worker that is loading is not doing that.

[memory.md](../research/memory.md#2-what-is-bounded-and-what-is-not) lists "one worker over the
ceiling, alone" as not bounded (**read**, from the code). What this report adds is the first
measurement of what that costs. Five workers of tracedecay were seen loading, all of them over the
ceiling that was set, none stopped by the daemon:

| Run | Ceiling set | Worker's footprint | What happened |
|---|---|---|---|
| u1 smoke (run A) | 24,576 MB (default) | 22,501 MB at 7 s of loading, 43,759 MB at 68 s | `blaze status` says "workers hold at most 24576 MB" beside the line that shows 43,759 MB |
| u1 run B | 24,576 MB | 59,648 MB (58.3 GB) when ready | swap at 20.4 GB right after |
| u1 run C | 24,576 MB | 56 GB (`top`: MEM 56G, compressed 48G) at about 12 min | swap 4.8 to 25.1 GB (harness sample); coordinator killed it |
| guarded run 1 | `RA_JIT_MEMORY_MAX=16G`, `RA_JIT_OWN_BASE_MEMORY=12G` | passed 16 GB at about 23:46:12, 24.0 GB at 23:46:29 | watchdog killed it; the daemon's log has no line about stopping a worker |
| guarded run 2 | the same | 16.0 GB at +199 s, 25.0 GB at 00:03:54; `blaze status` printed 16,776 MB at +203 s | watchdog killed it; the daemon's log again has no line about stopping a worker |

Effect: with one workspace, `RA_JIT_MEMORY_MAX` bounds nothing. The only things that stopped these
runs were the coordinator and the harness watchdog.

How it could be fixed is the daemon's to decide. Two shapes fit the code: the worker checks its own
footprint while loading, as the own-base thread does (`main.rs:9030`), and gives the load up past the
ceiling, so the build stays cargo's; or `fit()` includes a loading worker that is over the ceiling
and alone. Either keeps the rule that ra-jit never runs rustc: giving up a load leaves cargo's build
as it is.

**2. A loaded tracedecay needs more memory than this Mac has, so a correct bound would make blaze
unusable here.** The worker held 58.3 GB when it was ready (run B, 1 run; footprint, which counts
compressed pages). The Mac has 48 GiB. With gap 1 fixed and a ceiling of 24 GB, the worker would
never finish loading, every build would be cargo's, and blaze would only add the wrapper's cost
(item 2). The worker loads 586 crates, 1,019,121 symbols and about 20,000 macro expansions; which part
of the 58.3 GB each is was **not measured**. [memory.md](../research/memory.md) lists what shrinks a
worker, measured on other projects, not on this one. **Unverified** on Linux, where `VmRSS` is what the
daemon reads (`memory.rs:26-37`) and compressed pages do not exist; run B's RSS-based harness sample
was 24.5 GB for blaze's processes while the footprint was 58.3 GB, so the two readings differ by 2.4
times here and the Linux number is not the macOS one.

**3. Every edit goes to cargo.** 18 of 18 (item 4). The CLI's build script prints something else the
second time it runs, and blaze refuses to trust it and falls back to cargo "which ra-jit is never to
do" (the message's own words). Likely cause: the script's output depends on git's state (item 4,
**unverified**). This keeps blaze's edit loop at 3.4 to 11.5 times cargo's, where the point of blaze
is a faster edit loop, and each fallback starts another load (gap 1 again).

**4. The whole build fails on macOS arm64.** `BLAZE_WHOLE_BUILD=1` stops on tracedecay after 697 s and
on a hello-world after 8.8 s with `writable static ... without a rustc symbol` (`CLEANUP`,
`GLOBAL_PANIC_COUNT`, `OUTPUT_CAPTURE_USED`, `INSTANCE`: statics in the standard library's runtime,
reached from `lang_start`). The same error stops blaze's own compile of 107
build scripts in every build, cold or after an edit; blaze's message for each is that rustc compiles
the build script instead. The hello-world worker's log also lists 2,081 unsupported LLVM intrinsics
(`aarch64.sve.*`, `aarch64.neon.*`, `aarch64.crypto.*` in `core::core_arch`), which may or may not
matter (**not measured**). Everything in "ra-jit never runs rustc" depends on this one on a Mac.

**5. Cold builds through the wrapper cost 1.7 to 2.6 times cargo's (2.2 in the one run without
contention), and 9 to 13 GB more disk.** Item 2: 415 to 627 s against 232 to 239 s. The target
directory is 23.0 to 27.3 GB (n=4) against cargo's 14.2 GB (n=3), plus a store of up to 1.2 GB. In
the first seconds of every cold build 13 to 17 `ra-jit` processes that try to compile build scripts
each hold about 1.04 GB of footprint at once (16.2 GB at the peak in guarded run 2, **read**, 2 s
samples): a transient, but it is two thirds of the watchdog's 24 GB before any worker has loaded.

**6. `blaze build` waits for the load.** After cargo's `Finished` line the client printed `Waiting for
blaze to load the workspace` and did not return in runs B (the whole 1094 s), C (5.8 min, then killed)
and the guarded runs (2 min 6 s and 3 min 41 s, then killed), but returned at once in run A. Whether a build that
cargo has finished should wait for the daemon's load at all is the daemon's to decide; why A
differed is not explained.

**7. blaze compiles with Rust 1.99.0 whatever the project pins.** tracedecay pins
`nightly-2026-10-18` and fails under 1.99.0 at `tracedecay-code-index-runtime` (`queries overflow the
depth limit!`, item 1). One line fixes it in tracedecay; `BLAZE_OTHER_TOOLCHAIN=1` is documented to
build with the project's own toolchain and was **not measured** (the harness has a `blaze-nightly`
tool for it).

### Go or no-go

**No-go on this Mac.** blaze cannot yet do for tracedecay what cargo does here, in any of the three
things the program wants from it: it cannot build without cargo (gaps 3 and 4), it cannot rebuild
faster than cargo (gap 3 and item 4), and it cannot load the workspace inside the machine's memory
(gaps 1 and 2). The one thing it does faster, answering a build with nothing changed and running a
small crate's tests, is paid for with a worker that held 58.3 GB.

**On Linux: unverified.** Nothing was run there. For tracedecay to be a go on a Linux build host,
each of these would have to be true and measured with this harness and the watchdog:

1. A loaded tracedecay worker fits in the host's memory, and `RA_JIT_MEMORY_MAX` holds a worker that
   is loading, with the daemon's footprint read as `VmRSS` and no compressed memory to hide the rest
   (gaps 1 and 2). The host needs at least the 58.3 GB seen here, **unverified** for Linux.
2. The whole build (`BLAZE_WHOLE_BUILD=1`) builds tracedecay, with the build scripts. The docs say it
   does on small projects there ([self-contained.md](../self-contained.md)); on this Mac it does not
   even for hello-world (gap 4).
3. The CLI's build script prints the same thing twice, or blaze stops reading that as a reason to use
   cargo (gap 3), so that an edit is the daemon's.
4. The 1.99.0 toolchain builds tracedecay: one line added (gap 7).


## Commands

All from this checkout (`$H` is `python3 -I harness/tracedecay.py --td /Users/bytedance/dev/td-blaze-measure
--blaze-bin /Users/bytedance/dev/blaze-targets/u1/release --work <dir>`; `<dir>` is
`/Users/bytedance/dev/blaze-targets/u1-work` for u1's runs and `.../u1b-work/td` for u1b's). Every blaze
command ran under the guard and the watchdog from the first u1b run on; u1's did not.

u1 (reconstructed from `results.jsonl`, its logs and the harness; the shell history was not kept):

```
$H prepare
$H cold --tool cargo --runs 3                          # 3 runs, 231.8 to 238.6 s
$H cold --tool blaze --runs 1 --status                 # run A (a smoke run), run B, run C (killed)
$H noop --tool blaze --runs 3 ; $H noop --tool cargo --runs 3
$H edit --tool blaze --edit {tracedecay-body,mcp-body,signature} --runs 5 --status    # and the same with --tool cargo
$H cold --tool blaze-whole --runs 1                    # BLAZE_WHOLE_BUILD=1
$H test --tool cargo --package tracedecay-framing --runs 3 ; $H test --tool blaze --package tracedecay-framing --runs 3
```

u1b:

```
git switch -c dist/u1b-measure --no-track origin/dist/main                 # u1's branch name is taken by its worktree
cp -R .../u1-work/hello.noindex/{Cargo.toml,Cargo.lock,src} .../u1b-work/hello/

# the watchdog on a short run, default limit: no kill, peak 1,040 MB
$H --td .../u1b-work/hello --work .../u1b-work/test --package hello cold --tool blaze --runs 1 --status --stop
# the watchdog killing a run: limit lowered to 512 MB
$H --td .../u1b-work/hello --work .../u1b-work/test --package hello --watchdog-mb 512 cold --tool blaze --runs 1 --status --stop
  WATCHDOG: footprint of the blaze processes 1.1 GB passed 0.5 GB - killing 3 blaze processes
  {"tool": "blaze", "scenario": "cold", "run": 1, "wall_s": 3.04, "exit": -15, "outcome": "watchdog-killed", ... "rajit_footprint_peak_mb": 1109}
  blaze processes left afterwards: none

# the guarded cold builds of tracedecay (the only builds that ran at the time)
$H cold --tool blaze --runs 1 --status --stop                              # run 1, killed by the watchdog at 24.0 GB
$H cold --tool blaze --runs 1 --first-run 2 --status --stop                # run 2
$H test --tool blaze --package tracedecay-framing --runs 3                 # 596.9 s (contended), 0.48 s, 0.32 s
$H stop --tool blaze
$H summary                                                                  # per tool and scenario, with a killed column
```

The 512 MB limit is because a hello-world worker never holds 2 GB (1,040 to 1,109 MB). The footprint
traces of the cold runs came from a read-only helper that samples every 5 s and kills nothing, kept outside the
repository (the watchdog's own 2 s samples of runs 2 and the test runs are in `logs/watchdog-series.jsonl`);
the raw evidence of the guarded runs (daemon and worker logs, traces, `blaze status` polls) is in
`/Users/bytedance/dev/blaze-targets/u1b-work/td/run1-evidence` and `run2-evidence`, and u1's in
`/Users/bytedance/dev/blaze-targets/u1-work`. The swap rule of the
watchdog was exercised on fake inputs only (a unit check, not a run): no growth with 0.9 GB free did not
trip it, 1.5 GB of growth with 0.9 GB free did.


## What was not measured

- **Linux.** Nothing ran on Linux.
- **The three cargo and blaze edit loops with the guard.** u1's 18 edit builds and the cargo baselines
  were not repeated; they ran before the guard existed and are reported as u1 measured them.
- **A blaze cold build that completes under the guard.** The guard (24 GB) is below what the load
  needs (58.3 GB, run B), so no guarded run finished; guarded runs show the first 2.1 and 3.7 minutes of the load.
- **`BLAZE_OTHER_TOOLCHAIN=1`** (`blaze-nightly`): the harness has the tool, no run was made.
- **Why `blaze build` waited in runs B and C and not in run A**, why run A's cargo part was 519 s, why
  the target directory is 8.9 to 13.1 GB bigger than cargo's, and what part of blaze's edit time is the 107 failed
  build-script compiles.
- **The `queries overflow the depth limit!` text.** The log of that failing run was overwritten by the
  next run; the exit status and the failing crate are in `results-smoke.jsonl`.
- **The hello-world whole build's 8.8 s.** The worker's log has the error; the time is u1's.
- **The memory the load needs.** The worker was never allowed to finish under the guard; 58.3 GB comes
  from one unguarded run.
- **Windows and anything but Apple Silicon.**

