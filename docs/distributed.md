# Distributed cache, compile and tests

2026-10-26, revision 3.2. Status: design, nothing built. Written against the fork's `dist/main` at
`f9b5da46` (code as at `c8e5f6a8` plus u5's memory instruments; line numbers are those of
`f9b5da46`) and tracedecay at `d79de82273`. Each claim is marked **read** (in a source we read,
or measured by us), **reported** (a document or someone says so, not re-checked here) or
**unverified** (our inference or an estimate). A number taken from another document that states
no run count, spread or limiter is marked **n not stated**. Numbers name the file they come from.
Code paths are still `ra-jit` where the rename to blaze has not reached them.

Revision 1 (`45449ae6`) was rejected by both reviews of [PR
#1](https://github.com/ScriptedAlchemy/blaze/pull/1). Revision 2 (`2fd75c16`) was rejected by both
re-reviews: re-review A found 10 of 25 findings resolved, 13 partly, 1 not (a-B1) and 1 regressed
(b-M8), and seven new problems; re-review B found three blockers (the no-rustc boundary is not a
process tree, a work item cannot rebuild a generic instance on a helper, and the ahead queue
writes no linkable entries) and seven majors. This revision follows one rule: every blocker, every
finding not resolved and every regression is fixed here, in the design; every finding partly
resolved and every major is turned into acceptance lines of the unit that owns it, so the code
settles it, each line tagged with the finding's id. [Appendix A](#appendix-a-review-findings) gives
every finding of both re-reviews its status. It also takes in two reports written since revision
2: what holds a loading worker's memory ([reports/LOADMEM.md](reports/LOADMEM.md)) and why every
edit of the CLI went to cargo (`docs/reports/BUILDSCRIPT.md` on `dist/u8-buildscript`, PR #5,
parked after four failed verifications; P0.5 does its job in the whole build,
[reports/SCRIPTEDITS.md](reports/SCRIPTEDITS.md)).

**Revision 3.2** (this one) applies the owner's scope ruling of 2026-10-26: the program gives
tracedecay what Bazel with NativeLink gives it for Rust today, and nothing beyond. It adds the
section [Scope: parity with Bazel and NativeLink](#scope-parity-with-bazel-and-nativelink), marks
the pieces the ruling drops "out of scope (owner, 2026-10-26)" where they stand (the sandbox and
kernel enforcement of section 0, the isolation and capability design of section 2, the strict
test contract of section 6) and keeps only their parity remainder in the units. The design text of
a dropped piece stays as a record (in full in sections 0 and 2, summarised in section 6);
no unit builds it and no acceptance line depends on it.
Findings whose resolution needed a dropped piece carry the status "accepted at parity (owner scope
ruling)" in [appendix A](#appendix-a-review-findings). Nothing else of the design changed.

**Revision 3.1** applies the fixes of one focused check of revision 3 (`9875c09b`),
which judged the no-rustc boundary not workable as stated (B1) and the work key and retention
(B2), the exporter (B3) and the first-unit gate (D0) workable with fixes. Its findings carry the
ids `c3-B1.1` to `c3-D0.4`, tagged where each is fixed and in [appendix
A](#check-3-the-focused-check-of-revision-3). (Revision 3.2 drops the boundary altogether, see
above.) The boundary then claimed only what a kernel
or a profile could enforce: blaze never delegates Rust compilation, a worker exposes no host
toolchain, a job executes only approved code, and an in-process compiler is stated out of scope
(section 0). Work is named by a total key with a stated retention contract (section 4). The
exporter's context and the accepted closure are specified (section 4). GP is two stages with
named prerequisites (section 4, D0). No review cycle follows this edit.

Revision 2 took in the measurement of blaze on tracedecay
([reports/TRACEDECAY.md](reports/TRACEDECAY.md), G0), the survey of Bazel with NativeLink
([research/bazel-nativelink.md](research/bazel-nativelink.md), whose twelve edits are applied, see
[appendix B](#appendix-b-the-research-edits)) and the memory survey
([research/memory.md](research/memory.md)).

The goal: blaze gives tracedecay a distributed cache, distributed compile and distributed tests
for Rust on Linux x86-64 and macOS arm64, good enough that tracedecay drops NativeLink for its
Rust build and test actions there, and no more than NativeLink gives it (see the scope section).
Bazel keeps Windows, JS, Python, Clippy and the release and dist (thin LTO) builds. Workers run
blaze, never rustc ([no-rustc.md](no-rustc.md)).

Three decisions of the program's owner bind this design, and a fourth, the scope, has its own
section after them:

- **Distributed compile is required.** It means one build's compile work split across machines,
  not only whole jobs moved onto a fleet machine. A gate that fails changes how it is done, never
  whether it ships. Revision 1's "mode D is deleted if G4 fails" is gone.
- **Memory is guarded.** Every blaze run on the development Mac sets `RA_JIT_MEMORY_MAX=16G`,
  `RA_JIT_OWN_BASE_MEMORY=12G` and `RA_JIT_SPARE=0`, under a watchdog that kills the ra-jit tree
  (every process of the run, the client's descendants included) when its summed footprint (`top`'s
  MEM, not RSS) passes 24 GB, or when swap used has grown by at least 1 GB since the run started
  and free swap is under 3 GB, or when swap used has grown by 8 GB. (macOS grows swap on demand
  and other sessions keep free swap low, so free swap alone is not a signal; this replaces
  revision 2's unconditional 3 GB rule, re-review A N4.) No unit lowers these settings on the Mac
  without the owner's exception; a predicate that needs a smaller bound runs it on tracedecay at
  16G, or in Docker.
- **Heavy runs are serialised** by one lock on the development Mac; no dev host is reachable from
  it. Which gates can run where is part of each gate ([section 11](#11-gates)).

## Scope: parity with Bazel and NativeLink

**The ruling (owner, 2026-10-26).** The program adds nothing beyond what Bazel with NativeLink
already gives tracedecay for Rust. Where an earlier revision of this document asked for more, the
more is out of scope until the owner asks for it. A piece that is out of scope is marked "out of
scope (owner, 2026-10-26)" where it stands; its text is kept as a record of what was designed, and
no unit builds it.

**In scope (parity):**

- **A remote cache:** blobs, a per-crate lookup (blaze's own, since nothing blaze compiles can use
  NativeLink's entries, section 3), and GC with disk bounds on every tier.
- **Distributed compile:** one build's compile work split across machines (section 4). Bazel did
  this per action; blaze does it per batch of its own ahead loop. Required, never optional
  (standing order 13).
- **Remote test execution with cached results as Bazel's `cache_test_results=auto` caches them:**
  a pass is reused when the test's declared inputs are the same; a test carries an opt-out marker
  (Bazel's `external` and `no-cache` tags) and always runs; a failure is never cached; flaky tests
  are retried as `flaky_test_attempts` retries them (section 6).
- **A scheduler** with leases, retries, priority and capacity, as NativeLink's (section 5).
- **The access model of today:** tracedecay's `docs/ci-remote-builds.md` read at `d79de82273`
  (**read**). The server and the workers are reachable only on the tailnet; Tailscale ACLs, by
  tag, say who may reach what (the CI tag `tag:bazel-ci` reaches the cache and execution ports of
  the coordinator host and nothing else, and registers no worker); GitHub-hosted runners join
  the tailnet through the official Tailscale action with a GitHub OIDC identity that Tailscale
  checks (issuer, repository id, actor id, workflow file, audience), as today; fork pull
  requests are excluded and build with no remote and no fleet credentials. blaze adds no
  authentication of its own above that.
- **blaze compiling with itself.** blaze never hands its compile work to rustc, rustdoc or a cargo
  that runs them ([no-rustc.md](no-rustc.md)). This is how blaze compiles, not a sandbox, and it
  stays: the spawn rule for blaze's own children, the answerer for compiler probes and the audit
  that counts compiler execs in blaze's own run. So does the build-script runner that takes
  `cargo check` out of the whole build (P0.5, unit p05r).

**Out of scope, unless the owner later asks:**

- **Sandboxing or kernel enforcement against job code:** section 0's process-tree boundary, the
  exec store, Landlock, seccomp, the Seatbelt profile and the negative compiler fixtures. What job
  code (a test, a build script, a proc macro) runs is not policed beyond what Bazel did; a test
  that needs a toolchain declares it, as Bazel's `rust_test` runfiles did.
- **Per-executor OS users and disposable pull-request executors** (section 2's isolation rules).
- **App-level OIDC capabilities, `Main` and `Pr(n)` cache namespaces, server-signed records,
  epochs and trusted `Main` publishers** (section 2's capability, records and epoch parts).
- **A test contract stricter than Bazel's:** the always-run effect classifier, audit mode and
  sampling of cache hits, and the recording of what a test read (section 6's rules 1 and 2).

**What the dropped pieces would have protected against, and the accepted risk.** They would have
kept code from a pull request (its build scripts, proc macros and tests) from changing what a
build of `master` later uses: by writing the shared cache, by reading another job's files or a
worker's credentials on a shared worker, or by running a compiler of its own. Under parity pull-request
code runs on shared workers as the worker's user, and can write the shared cache, as it can with
NativeLink today (research 7.8: NativeLink serves `master` and pull requests from one instance).
**This is an accepted risk, equal to today's setup, by the owner's ruling.** Its limits are the
access model above: only the owner's own same-repository runs and the fleet's nodes reach the server.

**What parity means for the rest of this document.**

- The wire schema of section 8 and "Data shapes" is as unit C1 landed it. `Capability`, the
  record signature, `epoch` and `Namespace::Pr` stay in it as types; at parity the server
  issues no capabilities, signs nothing, runs one namespace (`Main`, epoch 0) that every
  permitted peer reads and writes, and does not require the credential headers of section 8. The schema's `role`, `trust` and
  sandbox-policy fields (an executor's `trust`, a `TestBaseKey`'s sandbox policy version) are
  constants at parity. Unit C3's acceptance says so for the server.
- "Trusted builder" and "pull-request executor" in sections 4 and 5 name a worker kept warm on
  `master` and a worker warm on a pull request's last push: warm front ends and memory sizing.
  The words no longer mean a separate user, sandbox or publication right.
- A request's principal is the tailnet peer. Who may write is Tailscale's ACL, not a record's
  signature (unit C6, reduced to that).
- Executors run jobs in a work directory as NativeLink workers do (units E1, E2, reduced).
- Phase 2's isolation tests, the adversarial harness and every acceptance line that needed them are
  gone; the findings they settled are in appendix A as "accepted at parity".

## Short version

- **Phase 0 comes first: blaze builds tracedecay.** G0 measured a no-go on macOS arm64: the
  whole build (no cargo) fails on a writable static of the standard library (#255); 18 of 18
  edits went to cargo because blaze reran the CLI's build script in an empty `OUT_DIR` and compared
  every byte it printed (BUILDSCRIPT.md, fixed by P0.5); a loaded tracedecay worker reached 58.3 GB
  on a 48 GiB Mac, which LOADMEM.md traces to the worker compiling the whole program at load in one
  salsa revision with every memo kept; `RA_JIT_MEMORY_MAX` does not bound a busy or last-asked
  worker (`daemon.rs:96-104`). Linux is unmeasured. Eight units fix and re-measure that before any
  distributed code depends on it, each closing a measurable predicate.
- **blaze never hands its compile work to rustc; that is a rule about blaze, not a sandbox.**
  Every process blaze itself starts goes through one spawn API that refuses a compiler, a cargo
  subcommand that compiles and a `RUSTC` that names one; compiler questions of build scripts
  (`-vV`, `autocfg`, `version_check`) are answered by blaze; an audit counts compiler execs in
  blaze's own run. What job code (a test, a build script, a proc macro) runs is not policed beyond
  what Bazel did. The process-tree boundary of section 0 (`noexec` mounts, an exec store, a seccomp
  filter, a Seatbelt allow-list) is **out of scope (owner, 2026-10-26)**.
- **Trust is the access model of today, not a new one.** The server and workers sit on the
  tailnet; Tailscale ACLs by tag say who may reach them; GitHub-hosted runners join through the
  Tailscale action with a GitHub OIDC identity; fork pull requests are excluded. The
  capability, namespace, signed-record, epoch and per-executor-user design of section 2 is **out
  of scope (owner, 2026-10-26)**, an accepted risk equal to today's NativeLink setup.
- **Distributed compile splits the build's own ahead loop into batches, over identical front
  ends.** A whole build compiles ahead from a queue of roots and the generic instances compiled
  functions name (`ObjectCodegen::compile_ahead`, `hir-ty-codegen/mod.rs:5479`); LOADMEM shows
  this loop is where a tracedecay build spends its memory, and unit u7 is bounding it in batches
  that release their memos. The distributed batch is that batch. Every executor of an execution
  loads the same `BuildSpec`, computes the same roots in one canonical order (by a total work
  key: unit, stable name and environment) and posts their digest, which is an admission check;
  roots are leased in ranges; an instance is compiled by an executor that discovered it and
  retains it, after the coordinator grants it the instance's work name. So no instance
  crosses the wire, only names and digests (re-review B, B2; c3-B2). A batch ends by exporting what it
  compiled as linkable store entries through a new exporter, and the lead assembles a
  store-hydrated full build that compiles nothing in the accepted set, the exported entries whose
  whole closure is reusable (B3; c3-B3). The first
  distributed unit is a two-process prototype on this Mac, and it gates the network layer. A lossless instance
  descriptor, which smaller helpers would need, is a later stage, built only if measurement asks
  for it. No system we read splits one crate's compile across machines (Bazel, NativeLink,
  sccache-dist, buck2 all distribute whole rustc invocations); this is new and its gate decides
  how, not whether.
- **The front end is what costs.** In u1's one tracedecay load, loading, resolving and expanding
  took 120 to 210 s (**read**, TRACEDECAY.md, two runs, contended); the resolution peak is 11.0
  to 13.1 GB live and 7.0 to 9.0 GB after it (**read**, LOADMEM.md, two runs, contended). So an
  executor pays only if its front end is warm or shared: builders are kept warm on `master`, a
  worker keeps its front end for the pushes that follow, and a front-end image of the dependency
  layer (#274) is mapped read-only instead of loaded.
- **Tests are cached as Bazel's `cache_test_results=auto` caches them.** A pass is reused when
  the test's declared inputs (its program, its runtime bundle, its arguments and declared
  environment, the platform) are the same. A test marked `external` or `no-cache` always runs.
  Failures are never cached. Flaky tests retry as `flaky_test_attempts` retries them. tracedecay's
  flake policy (fail) stays the owner's gate. The stricter contract of earlier revisions (the
  always-run effect classifier, audit mode, sampling) is **out of scope (owner, 2026-10-26)**.
- **Admission reserves measured memory and CPU** per executor and per job, master builds first
  (priority), a bounded queue, and stage timestamps on every job so a queue always has a named
  limiter. CPU and memory never enter a key. Disk is bounded on every tier, each with a default,
  a command and a check.
- **The wire request carries a resolved `BuildSpec`**: the unit graph after cargo's
  configuration and feature resolution, the profile, flags, environment, working directory, the
  run options of a test, and a digest of the native toolchain. A request blaze cannot honour is
  refused before it is scheduled.
- **Plan.** Thirty units in six phases, and one more (S4) if the owner chooses it: Phase 0
  (blaze builds tracedecay), the wire and the cache, execution and jobs (E1 and E2 reduced to
  executors that run jobs in a work directory, C6 to the tailnet's access model, T1 to Bazel's
  test-cache rules), distributed compile (its
  first unit, D0, the two-process prototype; D2, smaller helpers, only if measurement asks), tests,
  the CI switch. Each lands alone with a goal, files, acceptance and one verify command that
  checks all of it; acceptance lines that settle a review finding carry its id, as `[rb-M6]`. Owner-operated steps in tracedecay's own CI are listed apart from the units.

## What tracedecay runs today

All **read** from tracedecay at `d79de82273` unless marked.

- **The fleet** (as the owner reported it on 2026-10-26, **reported**): Ubuntu-main, 128 cores and
  256 GB, Linux (x86-64 by its name, **unverified**), the NativeLink coordinator and a Linux worker;
  a Mac Studio, M1 Ultra, 64 GB, macOS arm64, the macOS worker; a 12-core 64 GB machine and a
  12-core 16 GB laptop, operating system and role unknown (one of them is Omarchy, the second Linux
  worker in `docs/ci-remote-builds.md`). The Mac serves macOS actions only. Ubuntu-main is a large
  box; revision 1's "tracedecay has no large box" was wrong.
- **Hosted runners join over Tailscale** as ephemeral nodes tagged `tag:bazel-ci`, which may
  reach only Ubuntu-main TCP 50051 (cache and execution), 1985 (build events) and 443 (reports).
  Enrolment is bound to `repository_id` 1261602522, `actor_id` 25274700 and the `ci.yml` workflow
  ref. Fork pull requests are excluded by the workflow's `head.repo.full_name` test and run
  hosted-only (`docs/ci-remote-builds.md:29-45`, `ci.yml:345-348`).
- **One NativeLink instance, `main`, serves `master` and same-repository pull requests**, in
  plaintext gRPC over the tailnet. Bazel lets a client upload results of actions it ran locally
  unless told otherwise; whether the fleet's action cache refuses client writes is
  **unverified**. Workers that run pull-request tests also run `master`'s actions, with a shared
  hardlinked input cache (research/bazel-nativelink.md 7.8, **read** in NativeLink's sources). A
  malicious test could write a well-formed action-cache entry for a real action (**unverified**:
  each step is allowed by the sources; no attack was tried).
- **What runs remotely.** `--strategy=Rustc=remote` with `--noremote_local_fallback` ("Fleet
  compilation must not fall back onto the smaller hosted VM"), `--strategy=TestRunner=dynamic`,
  `--jobs=40`, instance `main` (`.github/actions/setup-bazel/action.yml:121-139`). Every Rust
  action carries `cpu_count=1` (tests 4) in its platform, and so in its key
  (`scripts/bazel/platforms/BUILD.bazel:9-13`; that it enters the key is REAPI's rule,
  research 7.7). `--jobs=40` is outstanding actions per invocation, not executing actions; the
  fleet's executing capacity is not in the repository (**unverified**).
- **Each Rust action declares 5.2 to 5.66 GB and about 33,000 input files**; rules_rust puts the
  whole Rust sysroot and C++ toolchain into every `Rustc` action; that this is most of the 5.2 GB
  is **unverified** (research section 1, 5).
- **Test shards.** A trusted run is one `fleet` shard; a hosted-only run is six, timeout 240
  minutes (`ci.yml:329-349`). `.config/nextest.toml:5-10` declares `retries = 1` with
  `flaky-result = "fail"`; the active Bazel workflow runs `//...` (`ci.yml:519-525`), so whether
  nextest's policy applies to it is **unverified**, but it states the owner's flake policy.
- **Tests that run rustc and cargo.** `scripts/bazel/rust_test.bzl:98-126` puts rustc, cargo,
  rustdoc and rust-analyzer on a test's `PATH` as runfiles. At least two tests run rustc and assert
  its diagnostics (`crates/tracedecay/tests/mcp_suite/mcp_handler_test/feedback_get_test.rs:37-64`,
  `crates/tracedecay/src/daemon/tests/feedback_impact.rs:352-375`) and one runs cargo on fixture
  projects (`crates/tracedecay-cli/tests/core_cli_suite/source_provenance_test.rs:527-561`)
  (**reported** by review B, with these lines).
- **Linux workers are hermetic** through a zig `cc` toolchain declared as an input
  (`.bazelrc:110-115`). The Mac has a newer Xcode and SDK than the hosted `macos-26` image, so
  matching OS and CPU does not make native compiles compatible (`docs/ci-remote-builds.md:10-12`).
- **Size.** 51 crates under `crates/`, 760 packages in `Cargo.lock` (revision 1 said 763; the
  pinned lock file has 760, review A m1). u1 built 572 crates for `-p tracedecay-cli`, and its
  worker loaded 586 (TRACEDECAY.md).
- **What one measurement found**
  (`.codex/pstack-runs/pr-merge-sweep/ci-scheduling-measurement-analysis.md`, untracked; one
  interrupted diagnostic run of a `k8-opt` release CLI build, run 39830079562, Bazel 9.3.1, no
  repeats): five uncached critical-path Rust actions spent 479.395 s queued and 352.217 s executing,
  so queueing was 51.8% of their 924.604 s; the `tracedecay` crate was one rustc action, 10.504 s
  queued and 200.846 s executing; a git submodule update took 487.687 s during analysis. The note
  says the queue's cause is unknown: "eligibility, admission, capacity, or fairness" were not
  recorded (line 67). This is a symptom of one release build, which this design leaves on Bazel. It
  is not a baseline and supports no capacity claim.

## What blaze has today

All **read** at `c8e5f6a8` unless marked.

### What G0 measured on tracedecay

From [reports/TRACEDECAY.md](reports/TRACEDECAY.md): Apple M5 Pro, 18 cores, 48 GiB, macOS 26.6.2;
blaze at `c8e5f6a8` built without LLVM; tracedecay `d79de82273` with
`#![recursion_limit = "256"]` added to `tracedecay-code-index-runtime`; `-p tracedecay-cli`, dev
profile; the machine was not quiet (Defender, Spotlight, another session's builds; runs marked
contended where another build peaked above 1 core).

| Question | Result | Runs, spread, label |
|---|---|---|
| Does the path without cargo build it? | No. `BLAZE_WHOLE_BUILD=1` fails after 697 s, `writable static GLOBAL_PANIC_COUNT without a rustc symbol`; a hello-world fails after 8.8 s with `CLEANUP` | 1 run each, **read** (the 8.8 s **reported**). Upstream [#255](https://github.com/indexable-inc/blaze/issues/255): the same static, every macOS doctest |
| Does `blaze build` build it? | Only as cargo with blaze as the rustc wrapper: 415 to 627 s to cargo's `Finished` (n=4) against cargo's 231.8 to 238.6 s (n=3) | contended in 3 of 4 blaze runs; limiter not measured, **read** |
| Who builds an edit? | cargo, 18 of 18: `the build script of tracedecay-cli printed something else` (`cargo/script.rs:682`) | n=5 per edit kind, **read**; why the script prints differently is **unverified** (its watch list depends on git state, `crates/tracedecay-cli/build.rs:417-422`) |
| How much memory does a loaded worker hold? | 58.3 GB footprint when ready (59,648 MB in `blaze status`); 22.0 GB at 7 s of loading and 42.7 GB a minute later | 1 run to ready, unguarded, **read**; Linux **unverified** |
| Does `RA_JIT_MEMORY_MAX` bound it? | No. With 16G set, the worker passed 16 GB and was killed by the harness watchdog at 24.0 and 25.0 GB; the daemon printed 16,776 MB beside "workers hold at most 16384 MB" | 2 runs, **read**; the cause is `over()` leaving out a busy worker and the one asked last (`cargo/daemon.rs:96-104`) |
| What does the load spend? | `load workspace` 79.9 and 159.2 s, `resolve: 586 crates` 20.4 and 26.4 s, `proc macros: 19987 expanded` 19.85 and 23.73 s; then `compiled ahead: 77345 from 77345 functions in 40.7s` | load phases from 2 guarded runs (run 2 contended), the ahead compile from 1 unguarded run, **read** |
| Disk | target directory 23.0 to 27.3 GB (n=4) against cargo's 14.2 GB (n=3); store 1.2 GB after a load | **read**; the extra 8.9 to 13.1 GB is not explained |
| A cold build's transient | 13 to 17 `ra-jit` processes compiling build scripts at about 1.04 GB each, 16.2 GB at the peak | 2 s samples, 1 run, **read** |
| What works | a build with nothing changed 0.20 to 0.58 s (n=3) against cargo's 3.13 to 8.86 s (n=3); `blaze test -p tracedecay-framing` 0.28 to 0.33 s (n=2) against cargo's 0.65 s (n=1), whose worker held 3.0 GB | **read**; the no-op builds came from the daemon holding the 58.3 GB worker |

The verdict there: no-go on this Mac. Every distributed unit below assumes blaze builds
tracedecay without cargo, within memory, and takes edits as the daemon's. Phase 0 makes those
true and measures them.

### What LOADMEM and BUILDSCRIPT found since

- **The 58.3 GB is the whole program compiled at load** ([reports/LOADMEM.md](reports/LOADMEM.md),
  u5, same Mac, two cold runs, contended, load average 25 to 42, **read**). After the front end
  (2.9 GB live at the end of a 97 s load, 13.1 GB at the resolution peak, 9.0 GB after it; the
  unprofiled run 2.4, 11.0 and 7.0 GB), 18 threads of `ObjectCodegen::compile_ahead` compile every
  instance the program reaches and keep every intermediate as a salsa memo: 21.2 GB of live growth
  in 40 s, about 25,000 functions a second and 23.1 KB of live heap for each; run 3 was killed at
  25.6 GB footprint with 640,000 functions compiled, run 2 at 33.3 GB. How many functions the whole
  build has is not known. The top
  holders are MIR (24%), compiled code and its edges (18%), bodies and item trees (10%).
- **Why the first build is the daemon's whole build:** 9 of tracedecay's 53 workspace units are
  `opt-level=1`, so blaze cannot patch cargo's executable (`calls_by_symbol`, `plan.rs:139,180,226`)
  and compiles the program itself (**read**, the `first build:` line, one run). For this design
  that build is the work to split, and its memory is a per-worker budget, not a fault.
- **u7 bounds it** (the program's unit u7-bound, running): `compile_ahead` takes its work in
  batches, and after each batch hands what it compiled on and releases its memos, against a real
  per-worker ceiling (LOADMEM section 6, fixes 2 and 3). Section 4 makes that batch the unit of
  distribution.
- **Edits went to cargo because of blaze, not the script** (BUILDSCRIPT.md, PR #5, `dist/u8-buildscript`,
  **reported** there: one run per case): blaze reran the script into an empty `OUT_DIR`, where
  cargo reruns it in the one it left, and compared every byte of its output, where cargo reads
  only `cargo:` lines. PR #5 reran in a copy of cargo's `OUT_DIR`, then in cargo's own, and
  failed four verifications on ever new interactions with the state cargo keeps in
  `target/debug`; the coordinator parked it. A worker of this design runs the whole build, which
  has no such state to agree with, so P0.5 moved there: the daemon asks for a script's run by
  cargo's own rule, and `harness/scriptedits.py` holds that to cargo 1.99.0
  ([reports/SCRIPTEDITS.md](reports/SCRIPTEDITS.md)).

### The store, the keys and the remote

- **The store** (`crates/ra-jit-store/src/lib.rs`): immutable packs named by their SHA-256, values
  in zstd blocks behind an index, one open pack per daemon (`seal_open` at `:320` seals it), LRU
  collection by pack (`collect` at `:1047`) within `RA_JIT_CACHE_MAX`, by default a tenth of the
  disk that is free or the store's, between 256 MiB and 32 GiB (`:89-96`). A key on disk is
  `sha256(identity ‖ key)`, where `Identity::new` hashes the store format, what blaze was built
  from (`built_from`, from `crates/ra-jit/build.rs`) and the toolchain. A published-crate
  install without `built_from` hashes its own executable instead (`:133-157`; review B M5).
- **The entry key** ([cache.md](cache.md)): backend and optimisation level, target triple, debug
  info, overflow checks and the function's stable name (`stable.rs`). A checkout crate is named by
  a hash of its source (`content.rs`); a dependency by its stable crate id. An edit renames the
  edited crate and every checkout crate that depends on it.
- **The home crate** (`hir-ty-codegen/home.rs`): an instance is compiled "from the point of view
  of" a crate that sees every crate its name mentions (`home.rs:265-294`), so `Vec<u8>::push` does
  not depend on the workspace crate that asked for it. That is what makes a compiled instance
  portable between builds. It is not the crate that wrote the function: `Vec<workspace::T>::push`,
  `T`'s drop glue and an iterator adapter over a workspace closure are homed in the workspace crate
  that defines `T` or the closure, whose dependency closure can be most of the program (re-review
  B, M2). So the home crate does not by itself make a helper's front end small.
- **The remote** (`crates/ra-jit/src/remote.rs`, `remote/`): `Dir`, `S3` and `Https` places;
  verified transfers with resume (`transfer.rs`); Ed25519-signed manifests with turns
  (`signed.rs`); remote GC by the clock (`collect.rs`, at least two hours' grace); a keeper that
  pushes and pulls in the background every five minutes and never makes a build wait
  (`keeper.rs:1-24`).

### What a build schedules

- One daemon compiles "a set of root functions into one object" (`CodegenUnit::compile`,
  `mod.rs:636`), each function a salsa query whose result lists its callees as requests
  (`fragment.rs`), on every core (`SymbolResolver::in_parallel`, `mod.rs:388`). A callee that
  cannot be compiled poisons its callers and the build restarts with it rejected
  (`compile_or_hold`, `:656`).
- **Compiling ahead.** Two loops share one shape. The build's own,
  `ObjectCodegen::compile_ahead` (`mod.rs:5479`, loop from `:5548`), runs on every core while the
  main thread puts functions in the object: threads pop a root (the build's exported and `main`
  roots, then an on-demand function when something calls it) or an instance, compile it
  (`compiled_root`, `compiled_instance`), and push every generic callee the result names, with its
  home (`Homes::home`), unless it was seen, is rejected, or the store has it with its whole closure
  (`in_store`, `Entries::usable`). A function that fails here is skipped; the main thread meets the
  error if the object needs the function. The daemon's spare-time loop, `compile_unnamed`
  (`mod.rs:2570`), does the same for every listed function and is speculative. Revision 2 named the
  second; the seam section 4 distributes is the first.
- **What the queue holds is local.** An item is `(Instance, StoredParamEnvAndCrate)`, interned ids
  of one salsa database (`mod.rs:2522`, `:3753`); a compiled function's requests name callees as such
  ids too (`fragment.rs`). The stable name (`stable.rs`) is a one-way spelling of an instance for
  the store's keys: a type spelled longer than 1,024 bytes is replaced by its hash
  (`stable.rs:40-46`), and only non-generic functions are looked up from a name (`:289-355`). A name
  cannot be turned back into an instance (re-review B, B2).
- **Store entries are made after stitching, not by the compile query.** `compiled_instance` keeps
  a `CompiledFunction` as a salsa memo. The linkable `cached::Entry` is made in `later.rs`
  (`keep` at `:300`, `entry` at `:408`) from a function already stitched into the object: whether
  each callee was imported, type-id hashes, the settled callee, debug type sets, patch tracking.
  And a build reads the checkout's code from the store only when it is the first full build
  (`hydrate`, `mod.rs:671`; `roots_from_store`, `:5206`); a patch neither compiles ahead (`:5486`)
  nor keeps entries (re-review B, B3).
- `RA_JIT_UNITS=1` groups functions into LLVM modules for whole-program builds; it is not the
  default, as it builds no faster (`units.rs:1-45`).
- One daemon stops scaling: compiling the standard library is 4.5 s on 16 cores and 3.9 s on 96,
  a fifth of the samples in rust-analyzer's interning and the lock slow paths beside it
  ([no-rustc.md](no-rustc.md), "The standard library, shipped"; dev-aws-8; run count not stated).

### Numbers kept from revision 1, with their provenance

Review A M2 asked every retained number to carry its machine, run count, spread and whether
cargo or rustc did part of it. These are background, not baselines: no ratio below is a claim of
this design. None of the source documents names a limiter or the commit measured for these rows;
where a cell gives no run count it is **n not stated**, and the same holds for every number this
design quotes from roadmap.md, ci-runner.md, benchmarks.md and research/codegen-units.md
(re-review A, a-M2).

| Number | Source | Machine, runs, spread | Hybrid? |
|---|---|---|---|
| ripgrep cold 16.8 to 17.4 s with an empty store, 3.98 to 3.99 s warm store, 4.01 to 4.02 s warm remote; nushell 78.9 s, 31.3 s, 31.5 s; helix 87.0 to 89.7 s, 25.5 to 25.7 s, 26.0 to 26.3 s | no-rustc.md, "The cold start without rustc" | dev-aws-8, 96 cores pinned; ranges over the runs recorded, run count not stated | no: "every crate ra-jit's and linked by ra-jit" |
| of nushell's 31 s warm: resolving 3.8 s, asking the store about roots 3 s, writing the object 7 s, listing 4.7 s, link 1 s | same | same; one breakdown | no |
| a fresh machine building cargo 0.92.0: 152.4 s no remote, 117.1 s with packs, 94.4 s with packs and cargo's units; lazy 100.2 s | [reports/REMOTE.md](reports/REMOTE.md) | loopback MinIO, one run each, no spread | **yes**: cargo ran rustc for dependencies and link lines, and three refused functions were compiled by rustc. Not all-blaze evidence |
| nushell body edit 258 ms; ready from nothing 52.1 s; 5.2 GB ready, 7.5 GB after its base, 11.2 GB peak | [roadmap.md](roadmap.md), [ci-runner.md](ci-runner.md) | machine and session not stated; n not stated; no spread | no |
| about 2 ms of thread time a function, 21.2 s on 16 cores for nushell's 165,752 | [roadmap.md](roadmap.md) | 16 cores; n not stated; no spread | no |
| sandbox about 16 ms a process on Linux | [ci-runner.md](ci-runner.md):388 (354 ms for 20 runs against 26 ms bare) | one measurement, no spread | no |
| slot sizes on Linux x86-64, 16 cores pinned: ripgrep to axum 1.4 to 2.0 GB, helix 5.7, nushell 7.1, bevy 15.2, biome 19.3, bun 24.5 GB | [research/memory.md](research/memory.md), from benchmarks.md | per-project rows of one bench run | no |

Revision 1's "eager fetch beats lazy" (94.4 against 100.2 s) is withdrawn as a conclusion: one
run each, hybrid.

## The rules this design keeps

- **blaze never hands its compile work to rustc.** blaze today reaches cargo and rustc from
  several places: a cold build dispatches to cargo (`cargo/mod.rs:265-285`), `by` starts `cargo
  build` or `cargo test --no-run` (`cargo/state.rs:490-504`), the wrapper runs the compiler
  (`cargo/wrapper.rs:150-151`), a build script blaze cannot build is left to rustc. Each is a
  `no-rustc` bug. Processes are also started from other crates and by raw `execve` (re-review B,
  B1, lists them). So Phase 0 routes every spawn of blaze's own through one API that refuses a
  compiler, answers compiler questions itself, and audits blaze's own run ([the
  rule](#the-no-rustc-rule-p0-as-mechanism)). The process-tree boundary earlier revisions put under
  it, which denied job code a toolchain, is **out of scope (owner, 2026-10-26)**: what a test, build
  script or proc macro runs is not policed beyond what Bazel did. In worker mode the rule cannot be
  turned off and a job is never turned green by cargo. `RA_JIT_RUSTC_BASE=0` does not disable
  cargo's dispatch (review A B1; it only stops patching a rustc-built base, `main.rs:3686-3695`),
  so revision 1's reliance on it is dropped.
- **Measurement before claims.** Every gate runs at least three times per arm, in one session, on
  the named machine, with median, spread and a limiter named from the phase lines
  (`RA_JIT_STATS`) or the job's stage timestamps. No ratio without a same-session baseline.
- **Where a gate runs** is one of: this Mac (heavy lock, guard), a Linux host at tracedecay scale
  (needs the owner's `linux-host` gate: SSH to Ubuntu-main or the 64 GB desktop), Docker's 8 GB VM
  on this Mac (small Linux fixtures only), or the fleet and tracedecay's CI (owner-operated; the
  unit lands its tooling and a report template, the owner runs it).

## Data shapes

Written as Rust. On the wire each struct is a CBOR map keyed by the small unsigned integers in the
comments, every key present (an absent `Option` is CBOR `null`), in ascending key order, the
encoding of RFC 8949 section 4.2.1 (shortest heads, definite lengths, no floats, no tags), as
[roadmap.md](roadmap.md#8-the-agent-api) requires. An enum is a two-element array `[discriminant,
payload]`, the discriminant the integer in its comment and the payload a map, or `null` for a unit
variant. A tuple is an array of its elements in order. A `Vec` marked **set** is sorted by the bytes
of each element's encoding and has no duplicates; any other `Vec` keeps its order. Strings are
UTF-8, at most the lengths `Hello` answers with.

A digest of a value is `sha256("blaze/" ‖ name ‖ "/v4" ‖ 0x00 ‖ canonical CBOR of the value)`,
where `name` is the type's name below. A signature is Ed25519 over the same bytes with `sig/` in
front of the name. One value signs less than its whole encoding: a `CrateRecord`'s signature is
over the six-field map of its fields 0 to 5, under the same `sig/CrateRecord` domain, so a record
carries a signature a client checks without field 6, which is the signature itself. Unit C1 fixes
these with golden vectors.

```rust
type Digest = [u8; 32];            // SHA-256

/// sha256(store FORMAT, built_from, toolchain commit): Identity in ra-jit-store, unchanged.
struct StoreIdentity(Digest);

/// The caller's key of one compiled function, unchanged (cache.md).
struct EntryKey(Digest);

/// The home crate of a set of entries (home.rs), under one profile:
/// digest of { 0: identity, 1: target triple, 2: entry settings, 3: crate's stable id }.
struct CrateKey(Digest);

/// What the native side of a build is: everything outside blaze that changes what is linked
/// or how a program runs. An executor serves a job only on an equal platform.
struct ExecPlatform {               // 0..=9
    triple: String,                 // 0, the blaze binary's target: no cross builds
    os: String,                     // 1, "macos 26.6.2", "linux 6.8"
    sdk: Option<(String, String, String)>, // 2, macOS: SDK name, version, deployment target
    libc: Option<(String, String)>, // 3, Linux: name and version, or the declared sysroot
    linker: (String, String, Digest), // 4, what links: name, version, digest of its executable
    cpu: String,                    // 5, target CPU and features codegen assumes
    cc: Option<(String, String, Digest)>,  // 6, the C compiler a build script runs, as the `cc` crate
                                    //    resolves it: name, version, digest of its executable
    cxx: Option<(String, String, Digest)>, // 7, the C++ compiler, the same way
    ar: Option<(String, String, Digest)>,  // 8, the archiver, the same way
    native_root: Option<Digest>,    // 9, the SDK (macOS) or the sysroot (Linux) by what is in it:
                                    //    a digest of the files that name its contents (C2 says which)
}

/// Which tree a job builds: a commit the server verified (section 2), or #271's root.
enum SourceRef {
    Git { repo_id: u64, commit: [u8; 20], tree: [u8; 20] }, // 0
    Root { root: Digest },                                   // 1, BLAKE3 Merkle root
}

/// A build after cargo's configuration, workspace, feature and profile resolution: what
/// `cargo blaze <command>` would do, with nothing left to the worker's environment.
struct BuildSpec {                  // 0..=12, schema version 4
    source: SourceRef,              // 0
    lock: Digest,                   // 1, sha256 of Cargo.lock
    units: Vec<UnitSpec>,           // 2, the resolved unit graph, in the canonical unit order
    asked: Vec<u32>,                // 3, set: indexes of the units the command named
    profile: ProfileSpec,           // 4
    platform: Digest,               // 5, ExecPlatform's digest
    rustflags: Vec<InputText>,      // 6, message-bounded flags, unchanged text encoding
                                    //    and config, only flags blaze honours
    config_env: Vec<(String, ConfigEnvValue)>, // 7, set, tagged resolved configuration values (v2)
    script_env: Vec<(String, ConfigEnvValue)>, // 8, set: fixed variables, native values preserved (v4)
    cwd: SourcePath,                // 9, relative to the workspace root, lossless byte components
    command: Command,               // 10
    toolchain: StoreIdentity,       // 11
    workspace_manifest: SourcePath, // 12, selected root manifest relative to SourceRef's root
}

```rust
struct SourcePath(String);          // schema 3: escaped byte-component logical path
enum ConfigEnvValue {
    Literal(EnvText),              // 0, [0, text], message-bounded NUL-free env value
    WorkspaceRelative(SourcePath), // 1, [1, path], joined to the selected workspace on the worker
    Native(NativeValue),            // 2, [2, bytes], NUL-free Unix environment bytes (v4)
}
struct InputText(String);           // same CBOR text, budget Limits.request, not identifier limit
struct EnvText(String);             // same CBOR text budget, environment-specific NUL exclusion
struct NativeValue(Vec<u8>);        // CBOR bytes, budget Limits.request, no NUL
```

Native env values must be non-UTF-8; valid UTF-8 has the single Literal encoding.
EnvText excludes NUL, and env maps reject empty/`=`/NUL names and repeated names.
script_env is original ambient input only and cannot contain WorkspaceRelative;
configured values stay origin-tagged in config_env. Only the local native probe
receives their materialized absolute paths, which never replace portable wire tags.

Schema 3 `SourcePath` losslessly escapes native component bytes. ASCII letters,
digits, `-`, `_`, `.` are literal; every other byte is uppercase `%HH`. `/`
separates components and `.` denotes the root. Percent, colon, literal Unix
backslash, Unicode and non-UTF-8 Unix bytes are reversible, not refused layouts.
Empty/dot/parent components, decoded slash/NUL, noncanonical escapes (including
escaped safe bytes) and absolute logical paths are malformed. Native conversion
must preserve one normal component; a host that cannot represent a component
reports a conversion error rather than silently reinterpret it. `cwd` is this
validated type at generation and decode; its workspace-relative meaning is unchanged.
`workspace_manifest` must end in the decoded basename `Cargo.toml`.
SourcePath text uses the negotiated message budget (default 16 MiB), not the
ordinary 4 KiB string budget. Escaping may triple native bytes. Typed validation
uses a bounded canonical structural pass then per-field budgets; bytes-only
validation retains ordinary string bounds and cannot identify path fields.
Generation validates the wire budget before creating a schedulable identity.
Git API contexts must supply their materialized repository root; only an intentionally
workspace-rooted Root snapshot permits an omitted source root.
Schema 2 replaces field 7's schema-1 string values with these explicit enum tags;
the old string encoding is rejected, never guessed from the text. Ambient overrides
of non-forced `[env]` entries are literals. A config-relative value is normalized
against the config file's origin and represented relative to the selected workspace.
Materialization consumes the serialized tag, not an out-of-band list of names.
The selected workspace is the parent of that manifest; replay first locates it
under the materialized source root, then joins `cwd` there and revalidates with
the explicit saved manifest. No repository-root fallback or project-name inference.

/// The canonical unit order: units sorted by the bytes of the canonical CBOR of each unit
/// without field 5. Every field but `deps` together identifies a unit (its dependencies follow
/// from them), so the order is total; `deps` and `asked` name indexes in it (re-review A N7).
struct UnitSpec {                   // 0..=8
    package: (String, String, Digest), // 0, name, version, source id digest
    target: (u8, String),           // 1, kind (0 lib, 1 bin, 2 test, 3 example, 4 bench,
                                    //    5 build script, 6 proc macro) and name
    features: Vec<String>,          // 2, set: after unification
    cfg: Vec<InputText>,            // 3, set: message-bounded cfg derived from flags
    edition: u16,                   // 4
    deps: Vec<(u32, String)>,       // 5, set: unit index and the name it is used under
    test: bool,                     // 6, cfg(test)
    host: bool,                     // 7, built for the host: build scripts, proc macros, their deps
    profile: ProfileSpec,           // 8, this unit's profile after [profile.*.package.*] and
                                    //    build-override (review B M4)
}

struct ProfileSpec {                // 0..=11
    name: String,                   // 0
    opt_level: u8,                  // 1, 0..=3; s and z refused until blaze has them
    debug: u8,                      // 2, 0 none, 1 line tables, 2 full
    debug_assertions: bool,         // 3
    overflow_checks: bool,          // 4
    panic: u8,                      // 5, 0 unwind, 1 abort
    strip: u8,                      // 6, 0 none, 1 debuginfo, 2 symbols
    split_debuginfo: u8,            // 7, 0 off, 1 packed, 2 unpacked
    backend: u8,                    // 8, blaze's codegen backend: 0 Cranelift, 1 LLVM
    mir_opt: u8,                    // 9, blaze's MIR optimisation level (cli.rs:66-102)
    backend_opt: u8,                // 10, LLVM's passes, 0..=3; 0 under Cranelift, which it does not change
    backend_emit: u8,               // 11, LLVM's code selection, 0 optimized, 1 fast; 0 under Cranelift
}                                   // lto other than off: refused (release stays on Bazel)

enum LocalPolicy { Never, Fallback, Race }  // 0, 1, 2

enum Command {
    Check,                          // 0
    Build,                          // 1
    Test(RunSpec),                  // 2
    Run(RunSpec),                   // 3
}

/// What a test run is asked to do. A timeout is not here: it schedules, and goes in the job
/// request (section 8), never in a key (re-review A N7).
struct RunSpec {                    // 0..=9
    filters: Vec<String>,           // 0, cargo test's TESTNAME words, in order
    exact: bool,                    // 1, read out of harness_args
    skip: Vec<String>,              // 2, read out of harness_args
    ignored: u8,                    // 3, 0 not ignored, 1 only ignored, 2 include ignored; read out of
                                    //    harness_args
    no_run: bool,                   // 4
    no_fail_fast: bool,             // 5
    test_threads: Option<u32>,      // 6, read out of harness_args
    harness_args: Vec<String>,      // 7, every word after `--`, as typed and in order, so that a custom
                                    //    harness gets exactly these (and for `Run`, the program's
                                    //    arguments before `--` too); 1 to 3 and 6 are read out of them
    env: Vec<(String, String)>,     // 8, set: the environment a test program is given
    doc: u8,                        // 9, doc tests: 0 none, 1 with the unit tests, 2 only these
}

/// The part of a BuildSpec a warm front end depends on: what an executor or an image
/// must agree with to take a job as an edit. digest of { 0: units with their features, cfg,
/// edition, deps, host flag and profile but not sources, 1: lock, 2: rustflags that set cfg,
/// 3: toolchain, 4: platform's triple }.
struct FrontEndKey(Digest);

/// The namespaces of the cache, and what may write each (section 2).
enum Namespace {
    Main,                           // 0, written only by trusted builders, at verified commits
    Pr(u64),                        // 1, written by that pull request's executors and runners;
                                    //    its payload is PrNamespace, the map {0: n}
    Std,                            // 2, the shipped standard library (shipped.rs), read-only
}

/// Which packs hold a crate's entries in a namespace, as one immutable generation.
struct CrateRecord {                // 0..=6
    namespace: Namespace,           // 0
    epoch: u64,                     // 1, the namespace's epoch when signed
    crate_key: CrateKey,            // 2
    generation: u64,                // 3, goes up by one per publish of this crate_key
    packs: Vec<PackRef>,            // 4, set
    source: SourceRef,              // 5, the verified commit whose build published it
    signature: [u8; 64],            // 6, by the server, over fields 0..=5, under the
                                    //    CrateRecord signature domain (section 2)
}

struct PackRef {                    // 0..=4
    pack: Digest,                   // 0
    size: u64,                      // 1
    keys: Digest,                   // 2, sha256 of its key list
    packed: Option<(Digest, u64)>,  // 3, the .zst object, when sent compressed
    format: u32,                    // 4, store FORMAT
}

/// What one build used: the GC's live roots are the entries builds used, not crate names
/// or whole generations (section 3, review B M9).
struct RootManifest {               // 0..=5
    namespace: Namespace,           // 0
    source: SourceRef,              // 1
    config: Digest,                 // 2, ConfigKey: what retention groups by (section 3)
    spec: Digest,                   // 3, BuildSpec digest
    used: Vec<(CrateKey, u64, Digest)>, // 4, set: crate, generation read or written, and the
                                    //    digest of the blob listing the entry keys used of it
    tests: Vec<TestKey>,            // 5, set
}

/// The BuildSpec without what changes from commit to commit: digest of { 0: command's kind,
/// 1: asked units' package names and targets, 2: each asked unit's profile, 3: platform,
/// 4: rustflags, 5: toolchain }. A commit's builds of one configuration share it.
struct ConfigKey(Digest);

/// A job's three identities (research 7.11, review A M6).
struct JobContent(Digest);          // digest of {0: BuildSpec digest, 1: Namespace}: what merges duplicates
struct ExecutionId(u64);            // the server's, one per execution of a JobContent
struct Attempt(u32);                // goes up when an execution is reassigned; fences events

/// What the server grants a principal for one job (section 2). Signed by the server,
/// short-lived, never written to a place a job can read.
struct Capability {                 // 0..=8
    principal: String,              // 0, "gh:run:<run_id>" or "builder:<name>" or "exec:<id>"
    role: u8,                       // 1, 0 requester, 1 executor, 2 trusted builder, 3 worker agent
    namespace: Namespace,           // 2
    source: SourceRef,              // 3, verified
    execution: Option<ExecutionId>, // 4
    attempt: Option<Attempt>,       // 5
    ops: u32,                       // 6, bits: 0 lookup, 1 fetch, 2 publish, 3 events, 4 cancel,
                                    //    5 roots, 6 delete a namespace, 7 work (join, lease, result)
    not_after_ms: u64,              // 7, wall clock, at most the job's deadline
    epoch: u64,                     // 8
}

/// A function of distributed codegen (section 4), as every executor of an execution names
/// it: the digest of its `WorkKey`. Total (every root and every discovery has a key, or a
/// terminal `Edge`) and environment-safe (two requests that may compile differently have
/// different keys). It is a name for coverage and grants, never a request another database
/// can rebuild (re-review B, B2; c3-B2.1).
struct WorkName(Digest);

enum WorkKey {                      // the digest's input
    Item {                          // 0, a function with a stable name
        unit: Digest,               // 0, the unit it is compiled in: digest of the canonical CBOR
                                    //    of its UnitSpec without `deps` (host and target are two)
        stable: Digest,             // 1, sha256 of its stable name (stable.rs): its store name
        env: Digest,                // 2, fingerprint of the asking param environment (caller
                                    //    bounds, opaque types, overflow mode), or the zero digest
                                    //    where the semantic layer proves the environment irrelevant
    },
    Edge { parent: WorkName, ordinal: u32 }, // 1, a request with no stable name (stable.rs:597-607):
                                    //    terminal `Local`, named by the function that makes it and
                                    //    its place among that function's requests
}

/// A name an executor found, and the digest of the complete local request it holds for it.
struct Found {                      // 0..=1
    name: WorkName,                 // 0
    tuple: Digest,                  // 1, the instance, param environment and crate spelled in full,
                                    //    with no 1,024-byte hash fallback: a digest, not a descriptor
}

/// What an executor computed from its front end before it may take work (section 4). It is an
/// admission check on the front ends, not a proof that they agree (c3-B2.3).
struct RootSet {                    // 0..=4
    spec: Digest,                   // 0, BuildSpec digest
    roots: Digest,                  // 1, digest of the build's roots as (WorkName, tuple) pairs,
                                    //    sorted by the bytes of the pair
    count: u64,                     // 2
    index: Digest,                  // 3, digest of the sorted WorkNames of the pre-index (section 4)
    inputs: Digest,                 // 4, digest of the content digests of every build-script output
                                    //    and proc-macro library the front end consumed, and of the
                                    //    export policy (section 4, "Export and ingestion")
}

/// An executor's tenure over the work it holds in one execution.
struct Lease {                      // 0..=3
    execution: ExecutionId,         // 0
    attempt: Attempt,               // 1, the execution's
    holder: String,                 // 2, executor id
    generation: u64,                // 3, new at every grant to this holder; fences its results
}

/// Work given to a holder: a range of the sorted roots, and names granted to it.
struct Grant {                      // 0..=2
    lease: Lease,                   // 0
    roots: Vec<(u64, u64)>,         // 1, set: half-open ranges of indexes into the sorted roots
    names: Vec<WorkName>,           // 2, set: functions granted to the holder: ones it found, and
                                    //    on-demand ones its pre-index resolves
}

enum ItemOutcome {
    Exported { entry: EntryKey },   // 0, compiled; its entry is in one of the result's packs. Raw:
                                    //    whether it is adoptable is decided by its closure (section 4)
    FromStore { entry: EntryKey },  // 1, the execution's pinned packs hold its whole usable closure
    Failed { why: String },         // 2, a CodegenError here; not a verdict on the build
    Local { why: String },          // 3, compiled but not exportable (the exporter's reason);
                                    //    the lead compiles it and what calls it
    Lost { why: String },           // 4, the holder cannot resolve the name: it no longer holds the
                                    //    finding (`lost-finding`), or the name is in neither its
                                    //    findings nor its pre-index (`unknown-name`, also counted as
                                    //    a conflict). It stops being a finder of the name
}

struct ItemResult {                 // 0..=3
    name: WorkName,                 // 0
    outcome: ItemOutcome,           // 1
    found: Vec<Found>,              // 2, set: generic instances and on-demand functions its
                                    //    code names, which the holder has retained and can compile
    unnamed: Vec<(u32, String)>,    // 3, set: ordinal and reason of each request of its code that
                                    //    has no stable name; the coordinator records each as a
                                    //    terminal `WorkKey::Edge` of this item
}

/// One batch's results, committed at once: outcomes, discoveries, packs and what it did not
/// get to (section 4).
struct BatchResult {                // 0..=5
    lease: Lease,                   // 0
    seq: u64,                       // 1, the holder's batch number; a replay of one is idempotent
    items: Vec<ItemResult>,         // 2
    packs: Vec<PackRef>,            // 3, sealed and uploaded before this is sent
    returned_roots: Vec<(u64, u64)>, // 4, set: granted ranges the batch did not process (budget)
    returned_names: Vec<WorkName>,  // 5, set: granted names the batch did not process
}

/// A test before it runs (section 6): what a fresh machine can compute without the read set.
struct TestBaseKey(Digest);         // digest of { binary digest, test name, RunSpec,
                                    //   platform, sandbox policy version, bundle digest }

/// What a run of a test observed. Published with its verdict.
struct InputManifest {              // 0..=3
    reads: Vec<Observation>,        // 0, set
    env_read: Vec<String>,          // 1, set: variables it read, all in RunSpec.env
    spawned: Vec<Digest>,           // 2, set: digests of programs it ran, all in the bundle
    policy: u32,                    // 3, sandbox policy version
}

enum Observation {
    File { path: String, digest: Digest, mode: u32, len: u64 }, // 0, path is logical
    Dir { path: String, names: Digest, mode: u32 },              // 1
    Absent { path: String },                                     // 2, a lookup that failed
    Link { path: String, target: String },                       // 3
}

/// digest of { TestBaseKey, InputManifest }: what a pass is kept under.
struct TestKey(Digest);

struct TestVerdict {                // 0..=6
    key: TestKey,                   // 0
    base: TestBaseKey,              // 1
    manifest: Digest,               // 2, InputManifest's digest; the manifest is a blob
    name: String,                   // 3
    passed: bool,                   // 4, only passes are kept; failures are reported, not cached
    duration_ns: u64,               // 5
    output: Option<Vec<u8>>,        // 6, failures only, bounded
}

/// What a test program needs to run on a machine that did not build it, by digest (Bazel's
/// runfiles; review B M6).
struct RuntimeBundle {              // 0..=5
    program: Digest,                // 0
    programs: Vec<(String, Digest)>, // 1, set: CARGO_BIN_EXE_* and other programs it may run
    libraries: Vec<(String, Digest)>, // 2, set: dynamic libraries outside the platform
    files: Digest,                  // 3, a tree of the fixtures and OUT_DIR files it may read
    cwd: String,                    // 4, logical
    env: Vec<(String, String)>,     // 5, set
}

/// What admission holds for an executor, a job or a test shard (section 5).
struct Reservation {                // 0..=2
    memory: u64,                    // 0, bytes: measured peak plus children plus headroom
    cpus: u32,                      // 1
    disk: u64,                      // 2, bytes of scratch
}

enum Stage {                        // stage timestamps, research 7.12
    Queued, Admitted, Assigned, Fetched, Loaded, Compiled,       // 0..=5
    Published, Linked, Tested, Reported, Done,                   // 6..=10
}

enum JobEvent {
    Stage { stage: Stage, at_ms: u64 },             // 0
    Assigned { executor: String, attempt: Attempt }, // 1
    Published { records: Vec<(CrateKey, u64)> },     // 2
    Program { name: String, blob: Digest },          // 3, a linked program, when asked for
    Verdict(TestVerdict),                            // 4
    Refused { function: String, why: String },       // 5, blaze cannot build it; the job fails
    Output { stream: u8, bytes: Vec<u8> },           // 6, what cargo would print, bounded
    Done { outcome: Outcome, counters: Vec<(String, u64)> }, // 7
}

enum Outcome {
    Passed,                         // 0, built, and every test run passed
    Failed,                         // 1, a build error or a failed test: a result, not retried
    Refused,                        // 2, blaze cannot build it; never handed to rustc
    Lost,                           // 3, the executor was lost twice
    Cancelled,                      // 4
    TimedOut,                       // 5
}

struct ExecutorState {              // 0..=6
    id: String,                     // 0
    trust: u8,                      // 1, 0 trusted builder, 1 pull-request executor
    namespace: Namespace,           // 2, Main, or the one Pr(n) it may ever serve
    front_end: Option<FrontEndKey>, // 3
    at: Option<SourceRef>,          // 4, the tree its front end has loaded
    reserved: Reservation,          // 5
    busy: Option<(ExecutionId, Attempt)>, // 6
}
```

What the server keeps on its own disk:

| Table | Key | Value | Mutable |
|---|---|---|---|
| `blobs/` | SHA-256 of the bytes | packs, key lists, manifests, bundles, executables | no |
| `records/<ns>/<crate_key>/<generation>` | `CrateKey`, generation | a signed `CrateRecord` | no; a new generation is a new file, written by conditional create |
| `latest/<ns>/<crate_key>` | `CrateKey` | the newest generation number | yes, compare-and-swap |
| `results/<ns>/<base>/` | `TestBaseKey` | `TestVerdict`s of passes, each with its manifest blob | append |
| `roots/<ns>/<config>/` | `ConfigKey`, `SourceRef`, spec | `RootManifest` | replaced per build of a commit |
| `used/` | digest | a sorted list of entry keys (a `RootManifest`'s `used` blobs) | no |
| `remap/<ns>/<crate_key>` | `CrateKey`, generation | the compacted generation that holds every live entry of it | append, by the collector |
| `epochs` | namespace | current epoch | by the operator only |
| `jobs`, `executors`, `work` | execution id, executor id | requests, leases, events, reservations, the work set | in memory, with a log |

## 0. Phase 0: blaze builds tracedecay

G0 says blaze does not build tracedecay without cargo on macOS, does not take its edits, and does
not hold its memory. Every phase after this one assumes all three. So Phase 0 is a track of its
own, first, with one predicate per blocker. Each predicate is measured with
`harness/tracedecay.py` (extended by unit P0.1), at least three runs per arm, guarded, and its
result is written into [reports/TRACEDECAY.md](reports/TRACEDECAY.md) by the unit that closes it.

| Predicate | Measured as | Bar | Where |
|---|---|---|---|
| **P0-A, no rustc** | the audit of blaze's own run (every exec in the daemon's and worker's tree with its parent chain), the stand-ins of `harness/norustc.py`, and the provenance record of every Rust code artifact the build links or loads; cold isolated target, store and daemon home | **positive:** `tracedecay-cli` cold build, first edit, test-binary build and run, macro expansion and a doctest succeed with 0 compiler execs started by blaze itself and every Rust artifact of blaze's origin, on both platforms; **negative:** each fixture of the rule's list (below) ends in a named refusal by blaze's own spawn API and the build failing only where it needed the refused spawn. The kernel and profile enforcement against job code, and the fixtures that tried it, are **out of scope (owner, 2026-10-26)** | this Mac; Linux at scale |
| **P0-B, memory bounded** | footprint (macOS) or `RssAnon` plus `RssFile` (Linux) of every worker and of each executor's whole process tree, 1 s samples; the daemon's log | with `RA_JIT_MEMORY_MAX=M`, the workers together hold at most 1.05 M for no more than 2 s in any state (loading, busy, last asked), and no single worker passes it; a worker stopped by the bound fails its build with the bound's message, and the build does not go to cargo; the harness watchdog never fires. Measured on `tracedecay-cli` at the guard's 16G, the case G0 failed | this Mac |
| **P0-C, tracedecay fits** | the same samples through a cold load, then 20 edit/undo pairs (memory.md M1) | a loaded `tracedecay-cli` worker holds at most 16 GB footprint on macOS arm64 at ready and after 20 pairs, so a guarded run completes; Linux's number recorded beside it | this Mac; Linux at scale |
| **P0-D, edits are the daemon's** | `blaze status` and the build logs: 5 runs, each of the three edit kinds of TRACEDECAY.md and its revert, after one cold and one warm build that are not counted | 30 of 30 counted builds (5 × 3 × an edit and its revert) `Finished ... [ra-jit]`, 0 `building with cargo`; the CLI's build script rerun when its watched inputs change and its new output used (G0's 18 failures are background, not this denominator; re-review B N2) | this Mac |
| **P0-E, the whole build on macOS** | `BLAZE_WHOLE_BUILD=1` | hello-world, ripgrep and `tracedecay-cli` build and their programs print what cargo's do; 0 `without a rustc symbol`; `harness/doctestonce.py` passes on macOS (#255) | this Mac |
| **P0-F, a cold build's peak** | the watchdog's 2 s series | the transient of script compiles at most 4 GB footprint (16.2 GB today) | this Mac |
| **P0-G, Linux at scale** | P0-A to P0-F on Linux x86-64 | each bar met | a Linux host of at least 64 GB (owner's `linux-host` gate) |
| **P0-H, G0 again** | TRACEDECAY.md's scenarios: cold, edits, no-op, test, disk | blaze's cold whole build and each edit kind measured against cargo in the same session, n ≥ 3, limiter named. No speed bar: Phase 0 is about building, not winning | both platforms |

What is decided here, and why:

- **A loading worker is bounded by checking itself, and its build is bounded in batches**
  (TRACEDECAY.md's first shape, LOADMEM section 6): the worker reads its own footprint while it
  loads, as the own-base thread does (`main.rs:9048-9068`), and past the ceiling stops with a named
  reason; `compile_ahead` checks the budget before each job and, past it, ends the batch, hands what
  it compiled on and releases its memos. `over()` is also changed so a worker alone over the ceiling
  is not exempt, and the ceiling stays what the daemon's workers hold together. In worker mode the
  stop fails the job; locally the daemon tells the user, as it does today, and the no-rustc rule
  decides what happens next. Unit P0.3, which the program's unit u7-bound is implementing.
- **The worker gets smaller by LOADMEM's ranked fixes, then memory.md's list**: the bounded batch
  (above), MIR that is not kept once an instance has code, eviction during resolution, the type GC
  and the allocator's purge between batches, the driver's maps keyed by hashes, the symbol table as a
  view of its file; then reclaim at idle, recycle on growth, one worktree path per executor. P0-C's
  16 GB is set by the guard: a worker larger than that cannot be verified on this Mac at all. If the
  owner grants a Linux host instead, P0-C keeps its bar on macOS (the Mac Studio has 64 GB, section
  4) and adds Linux's. Unit P0.4.
- **A build script is rerun as cargo reruns it.** blaze reran a script into an empty `OUT_DIR` and
  refused it if any byte of its output differed (`cargo/script.rs:670-688`); cargo reruns in the
  `OUT_DIR` it left and reads only `cargo:` lines. BUILDSCRIPT.md found that the CLI's script runs
  `pnpm` in an empty `OUT_DIR` and prints 61 lines of its own there, so every edit went to cargo; it
  is blaze's bug, and tracedecay needs no change. PR #5 (parked, above) tried it beside cargo's
  builds. P0.5 now compiles and runs scripts in blaze's own whole-build runner
  (`crates/ra-jit/src/scripts.rs`, units p05w and p05r), with cargo's freshness rules
  (`blaze_build_script::fresh`) and the variables of the process that asks. A run uses the
  `OUT_DIR` it left and is not replayed from the store. Graphs with proc macros still use the
  load's `cargo check`, which also runs their scripts; removing that remaining check is
  no-rustc step J ([reports/SCRIPTEDITS.md](reports/SCRIPTEDITS.md)). A script whose cargo-read
  output differs between two runs on the same inputs is impure: run once per build, its latest
  output used (recording its reads where blaze's existing sandbox does, #147), and its run never
  shared across machines (section 1). Unit P0.5.
- **The standard library's writable statics are blaze's on macOS** (#255): `CLEANUP`,
  `GLOBAL_PANIC_COUNT`, `OUTPUT_CAPTURE_USED`, `INSTANCE` have no rustc symbol in the toolchain's
  `libstd` dylib on macOS (`nm -m`: non-external, #255, **reported**). The whole-program path
  owns them on Linux. Unit P0.6 owns them on macOS the same way.
- **tracedecay needs one line under Rust 1.99.0**: `#![recursion_limit = "256"]` in
  `tracedecay-code-index-runtime` (TRACEDECAY.md item 1). That is a change to tracedecay, the
  owner's, not a unit here. asupersync's `nightly-outcome-try` probe falls back on stable with a
  warning (TRACEDECAY.md), so tracedecay does not need nightly to build (**read**, one build).

Distributed units depend on Phase 0's predicates by name in the plan below.

### The no-rustc rule (P0-A's mechanism)

Re-review B (B1) showed that a gate on blaze's own `Command::new` is not "every process blaze
starts": `sandbox.rs:643,682` forks and calls `execve` itself; `blaze-build-script` runs scripts and
tools (`run.rs:39,79-84`, `main.rs:104-107`); the vendored `proc-macro-api` starts the macro server
(`process.rs:115-117,416-438`); `project-model` runs `cargo check --compile-time-deps`
(`build_dependencies.rs:442-464,492-494,580-582`) and `cargo metadata` (`cargo_workspace.rs`);
`asm.rs:1088-1096` runs an assembler; `blaze-registry/src/git.rs:212,463,487` runs git, which can run
configured helpers; `whole.rs:322-331` runs the built program; `main.rs` links through `sh -c`;
`answers.rs:165-166` still falls back to the real rustc. Descendants of a job (build scripts, proc
macros, linker wrappers, tests) start what they like; policing them is out of scope (owner,
2026-10-26, below). The rule is therefore about every process blaze itself starts, and has two
parts and an audit.

1. **Blaze's own spawns go through one API**, `crates/blaze-spawn` (new, small): it gives a named error for a known
   compiler, refuses cargo subcommands that compile (`build`, `check`, `test`, `run`, `rustc`, `doc`, `bench`, `install`, `fix`, `clippy`, `miri`), `cargo
   metadata` without `--no-deps`, a cargo alias that expands to one of those (read from cargo's
   config), an external `cargo-*` subcommand, and a `RUSTC`, `RUSTC_WRAPPER` or
   `RUSTC_WORKSPACE_WRAPPER` that names a compiler, before the spawn, with a named error. Every
   spawn site above goes through it, the vendored ones by one hook in `stdx::process` and
   `proc-macro-api`. It is the rule's enforcement, with the audit: it gives a named error before any
   exec, and covers a cold dispatch that never reaches one. `harness/spawnsites.py` lists every
   process-creating call (`Command::new`, `libc::{fork, vfork, execve, execvp, posix_spawn*}`) in
   `crates/` and `vendor/rust-analyzer` and fails on one outside `blaze-spawn` that
   `harness/dist/spawn-allow.toml` does not list with its reason.
2. **Compiler questions are blaze's.** A build script's `RUSTC` and `RUSTDOC` point at blaze's
   answerer (`ra-jit-rustc`, `answers/own.rs`), which answers `-vV`, `--print` and `--version`
   itself and checks the probe programs of `autocfg`, `version_check` and `rustversion` with blaze's
   own front end; the fallback to the real rustc (`answers.rs:165-166`) is removed. A question it
   cannot answer fails the probe with a named error, never by asking rustc.
3. **The audit and provenance (re-review B, M5; reduced to blaze's own run).** Zero compiler
   execs in an audited interval does not prove the build was blaze's: a warm target or store, a
   rustc-built base executable, script executable or macro library would let a hybrid build pass.
   So P0-A runs with a cold isolated target, store and daemon home; the build records the origin
   and digest of every Rust code artifact and macro server it links or loads; and the audit fails
   on any origin that is not blaze's. The audit's sources are the spawn API's log of every spawn
   blaze makes, the counts of the stand-in `rustc`, `rustdoc` and `cargo` of `harness/norustc.py`
   on `PATH` and as `RUSTC`, `RUSTDOC` and `CARGO`, and `harness/spawnsites.py`'s static list of
   every process-creating call. The paths that reach rustc-built artifacts today are removed in
   worker mode: `own_macro.rs:49-73` (the toolchain's macro server when a shipped one is absent)
   and `:86-119` (the rustc-built macro library after a failed own build), the symbol files
   `main.rs:7739-7746` loads, and `whole.rs:37-47`, which makes Build and Run whole but not Test
   and Check. "0 compiler execs" is the oracle for blaze's own run only; it is not a claim about
   what a test, build script or proc macro does, which is not policed beyond what Bazel did.

**Fixtures of the rule** (`harness/dist/fixtures/norustc/`, both platforms), each ending with a
named refusal from blaze's own spawn API or answerer, 0 compiler execs started by blaze, and the
build failing only where it needed what was refused: a project whose model needs `cargo check
--compile-time-deps` (project-model, `build_dependencies.rs:442-464`); `cargo metadata` without
`--no-deps`; a cargo alias `b = "build"` and `cargo b` asked of blaze's own spawn; a `cargo-foo`
plugin asked of it; `RUSTC_WRAPPER`, `RUSTC_WORKSPACE_WRAPPER` and `RUSTC` naming a compiler; a
cold dispatch that would reach cargo; a probe question the answerer cannot answer. Positive
fixtures: the CLI's build script running `pnpm`; `autocfg`, `version_check` and `rustversion`
probes answered by blaze; a C dependency built with `cc`; a blaze-built test program run, a
blaze-built proc-macro library loaded; tracedecay's 107 build scripts, each run by blaze.

#### Out of scope (owner, 2026-10-26): the process-tree boundary

Revisions 3 and 3.1 put a boundary under the rule: an OS boundary around the worker and every
process under it, in which the host toolchain is absent or unreadable and the kernel refuses to run
or map a file that is not approved (Linux: `noexec` mounts, an exec store, Landlock, seccomp; macOS:
a Seatbelt allow-list), failing closed. The owner ruled that out: it is sandboxing against job
code, which Bazel with NativeLink never gave tracedecay (see [the scope
section](#scope-parity-with-bazel-and-nativelink)). **No unit builds anything below and no
acceptance line depends on it.** It is kept as a record, and as the place to start if the owner
asks for it later. Its text refers to "the boundary", "the exec store", "approved code" and
"promotion"; none of these exist at parity.

- **The tree reaches no Rust toolchain and runs only code blaze approved** (the boundary
   itself). It is in place before the worker loads anything: on a fleet machine the agent sets it
   up as it starts the executor (on Linux the agent, which creates the executor's user, creates its
   namespaces, E1); locally the daemon installs it on itself. Every descendant inherits it and
   cannot drop it.

   **What it guarantees, and what it does not (c3-B1.1).** The guarantee is the three statements
   below. Revision 3 claimed more: that a denylist of compiler names, digests and
   `librustc_driver` linkage stops any compiler a job supplies. It does not, and the claim is
   withdrawn. A compiler of another build, a rebuilt or statically linked one, or a
   driver renamed with its dependency edited has no known name, digest or needed library; a
   compiler inside a test, build script or proc macro has no exec at all.

   - **blaze never delegates Rust compilation.** No path of blaze hands a function, crate, build
     script, proc macro, check, test, doctest or link to rustc, rustdoc or a cargo that runs them.
     Layers 2 and 3, the audit and `harness/spawnsites.py` enforce it.
   - **A worker exposes no host Rust toolchain to a job.** No mount, mapping, file descriptor or
     path of `$RUSTUP_HOME/toolchains/*`, `$CARGO_HOME/bin`, the sysroot's `bin`, the rustc-built
     libraries of `lib/rustlib/<triple>/lib` or any `librustc_driver*` reaches the job's tree.
     rust-src and blaze's shipped standard library are mounted read-only; they are sources and
     blaze's own artifacts, not a toolchain.
   - **A job executes and maps only approved code; everything else is denied by default.**
     Approved is: an immutable trusted native tool (the programs of the `BuildSpec`'s native
     toolchain digest and of a `RuntimeBundle`, by digest), or a binary or library blaze built or
     promoted (below), with its origin and digest recorded. The test is on the file the kernel
     runs or maps, never on its name.

   **Out of the boundary's scope, stated plainly.** Arbitrary job code can implement a Rust
   compiler in its own process, or bring one in its own bytes (in a fixture, constructed at run
   time, or fetched through a content channel the job is allowed). A test, build script or proc
   macro that does so is a program blaze built, and the superset rule says it behaves as under
   cargo. No OS mechanism tells it from any other program; preventing it would need semantic
   mediation of the job's code, which this design does not do and no owner decision asks for.
   Tests that run rustc or cargo themselves are the owner's `rustc-tests` gate (section 6). So
   "0 compiler execs" is not an oracle that no compilation happened: P0-A's oracle is the
   refusals the fixtures name and the provenance record of every Rust code artifact linked or
   loaded (below). The old checks, a compiler's name, a digest in the toolchain manifest, an ELF
   needing `librustc_driver-*`, stay as a diagnostic only: they give a job that runs a known
   host compiler a named error, and a declaration check refuses to approve such a file for a
   bundle. They are not the boundary.
   - **Linux:** the worker runs in its own mount namespace (`sandbox.rs`'s user and mount
     namespaces, extended from one test to the worker's whole tree; E1 later makes it the job
     sandbox). The host toolchain is not mounted. Exec is authorised by the kernel over objects the
     job cannot change, never by the supervisor (c3-B1.2):
     1. **Mutable storage cannot run code.** Every mount the job can write (worktree, target
        directory, `OUT_DIR`, overlay, temporary directories, `/dev/shm`) is `noexec,nosuid,nodev`.
        The kernel then refuses `execve` of a file there, `execveat` of an fd of one, and
        `mmap(PROT_EXEC)` of one, which stops an explicit dynamic loader run on a job's binary
        (`ld.so ./prog`) and a `dlopen` or `LD_PRELOAD` of a job's library. Approved code lives only
        on read-only mounts: the trusted native tools, the shipped standard library and the exec
        store. Where the kernel has Landlock, `LANDLOCK_ACCESS_FS_EXECUTE` is granted to those
        hierarchies alone, as a second kernel check.
     2. **Promotion is the one way in, and it is outside the job.** The exec store holds
        content-addressed files, mode 0555, owned by the control user, on a mount the job sees
        read-only and executable, and only the supervisor side writes it, recording each file's
        origin and digest. It takes two kinds of file. *What blaze built*: test and build-script
        programs, the program itself, proc-macro libraries; a path the job knows
        (`CARGO_BIN_EXE_*`, `target/debug/<name>`) resolves into the store. *What a trusted native
        tool produced for the job*: scripts and tools reach the C toolchain through blaze's `cc`,
        `c++`, `ld` and `ar` wrappers (`CC`, `CXX`, `LD`, `AR` and the front of `PATH`); a wrapper
        asked for an executable or a shared library sends its command line to the supervisor over
        a descriptor that authorises only that request, the supervisor runs the real tool outside
        the job's tree on the job's inputs and publishes the output, and the wrapper leaves a
        symlink to it at the output path, so `./probe` and a `protoc` built by `cmake`
        (`protobuf-src`) still run. A job cannot promote a file it wrote: the store takes only what
        the supervisor itself produced. A compliant program that executes a native file by another
        route (a Rust-written ELF, a download, a checked-in binary) is refused with a named error
        and the file's digest, and runs once its bundle declares that digest. P0.2 measures which
        of tracedecay's 107 build scripts and of the popular `-sys` crates need which route; a
        compliant program the routes do not cover is a bug of the promotion path, or the sandbox
        difference [#147](https://github.com/indexable-inc/blaze/issues/147) already allows, and is
        never refused to strengthen the claim.
     3. **No exec-able anonymous file.** `memfd_create` is allowed only with `MFD_NOEXEC_SEAL`
        (the filter rejects the call otherwise, and the sandbox sets `vm.memfd_noexec=2` in its
        pid namespace where the kernel has it); `execveat(fd, "", AT_EMPTY_PATH)` and `fexecve` on
        an fd of a memfd, of a file on a `noexec` mount or of a deleted file are refused by the
        kernel, and on an fd of an approved file are the approved inode and allowed. The worker
        closes every inherited descriptor above 2 (`close_range`) before any job code, so no
        descriptor of the host toolchain survives into the tree.
     4. **Scripts and interpreters are explicit.** A `#!` script needs execute permission on an
        exec mount, and so does the interpreter its first line names, so a script a job wrote
        cannot be run by name. `sh script`, `python3 script` and `perl script` read the script as
        data: the interpreter is an approved tool and what it runs is the job's code, which is the
        out-of-scope case above, not a new exec. A script on an approved mount names only an
        approved interpreter.
     5. **The supervisor audits and fails closed; it does not decide by continuing.** The seccomp
        user-notification supervisor records every exec with its process chain, and reads
        `/proc/<pid>/exe` and `maps` after the exec for what ran. `CONTINUE` after reading a path
        is never a security decision: the pathname bytes are the job's memory, another thread can
        replace them or a symlink between the supervisor's read and the kernel's open, and an
        fd-based exec has no last component to read. A target the supervisor cannot read (an
        unreadable pointer, an over-long path) or an audit it cannot write is denied with `EACCES`.
     6. **Unknown ABIs are rejected.** The filter kills the process (`SECCOMP_RET_KILL_PROCESS`) on
        any `seccomp_data.arch` other than the native one, including 32-bit compat and, on x86-64,
        an `x32` number (`nr & 0x40000000`). Today a non-matching architecture is allowed
        (`sandbox.rs:695-705`), and `execveat` is listed but its empty pathname is skipped
        (`:835-858`).
     7. **Before any untrusted work** the sandbox sets `PR_SET_NO_NEW_PRIVS`, drops the capabilities
        that remount, enter or create namespaces, trace or change roots (`CAP_SYS_ADMIN`,
        `CAP_SYS_PTRACE`, `CAP_SYS_CHROOT`, `CAP_SETPCAP`) from the bounding set, and the filter
        denies `mount`, `umount2`, `pivot_root`, `move_mount`, `open_tree`, `fsopen`, `fsmount`,
        `unshare`, `setns`, `ptrace`, `process_vm_writev` and `clone`/`clone3` with a `CLONE_NEW*`
        flag, so a job cannot remount a writable mount `exec` or enter a namespace in which it can.
   - **macOS:** the worker applies a Seatbelt profile to itself (`sandbox_init`, inherited by every
     descendant). It is a deny-by-default allow-list, not a denylist: `process-exec` and
     `file-map-executable` are allowed only for subpaths the job cannot write and that another
     user owns (the trusted native tools, the shipped standard library, the exec store of
     step 2), so a file a job wrote, a copy of a compiler included, cannot be executed or mapped;
     `file-read-data` is denied for the host toolchain's directories and any `librustc_driver*`;
     rust-src stays readable. Seatbelt is path-based, with no per-inode rule; the allow-list
     stands because the job cannot create or replace a file inside an allowed subpath
     (**unverified**: P0.2's probe tries a hard link, a rename and a symlink into one). Denials are
     reported with Seatbelt's `report` action into the audit. The C toolchain, git and other
     tools stay allowed: tracedecay's CLI build script runs `pnpm`. There is no
     `no_new_privs` or capability set to drop on macOS: the executor is an unprivileged user per
     executor (E2) and the profile denies the task-port and debugger operations on other
     processes (the operation names are fixed by P0.2's probe, **unverified**).
   - **Fails closed on workers.** An executor whose boundary cannot be set up (no user namespaces,
     no seccomp user notification, a kernel that cannot make a mount `noexec`, or an ABI filter
     that does not load, on Linux; `sandbox_init` failing on macOS) does not start, naming
     why. Locally, the daemon installs the same boundary over build-time descendants (build
     scripts, proc-macro servers, linkers, blaze's own children) where it can; where it cannot (a
     Linux that forbids unprivileged user namespaces, as Ubuntu's AppArmor setting can, **unverified**
     here) it builds with the spawn API's refusals and the audit alone and says the boundary is off.
     `BLAZE_NO_RUSTC=audit` records instead of denying, for diagnosis; a worker ignores it.
   - **What the boundary covers.** On a worker, everything, tests included: a worker runs blaze,
     never rustc. Locally a test program the user runs is the user's program: if
     it runs rustc at run time, that is the program's behaviour, not blaze compiling with rustc, and
     the superset rule says it must behave as under cargo; the audit reports it. This is why
     tracedecay's tests that run rustc or cargo need the owner's `rustc-tests` gate (section 6).
     The security of what a pull request runs is section 2's isolation, not this boundary's.

**Negative fixtures of the dropped boundary** (`harness/dist/fixtures/norustc/`, both platforms, not built). Two kinds, with the
outcome each is expected to have under the guarantee above (c3-B1.1).

*Host toolchain and delegation, refused:* a build script that runs rustc by absolute path; a copy
of the host's rustc (the copy cannot be read on macOS, and is not mounted on Linux); `sh -c 'rustc
...'`; a linker wrapper (`-C linker=./cc-wrapper`) that runs rustc; a build script that runs
`cargo build`; a proc macro that runs `rustc -` on source; a test that runs `cargo metadata`,
`cargo build` and `rustc`; a cargo alias `b = "build"` and `cargo b`; a `cargo-foo` plugin on
`PATH` that runs rustc; `RUSTC_WRAPPER` pointing at rustc; project-model's compile-time
dependencies; git with a credential helper that runs rustc. Each ends with 0 compiler execs, a
named refusal in the audit, and the build failing only where it needed what was refused.

*Compilers the job brings, and the outcome the narrowed guarantee gives each.* The oracle is the
named kernel or profile refusal (the errno and the audit line naming the file) and the provenance
record, not a count of execs: a library mapped into an approved process has no exec at all.

| Fixture | Linux | macOS |
|---|---|---|
| a **pre-staged non-host toolchain**: an unmodified rustc and `librustc_driver` of another version, in the fixture's tree or in a bundle directory not declared | refused: not on an approved read-only mount, `EACCES` from the `noexec` mount | refused: `process-exec` outside the allow-list |
| the same toolchain **declared** in a bundle | refused at admission by the declaration check (a diagnostic, not the boundary); if the check does not recognise it, it is approved: a bundle is the owner's act, not a job's | same |
| a **renamed compiler plus driver**: rustc copied to `cc1`, `librustc_driver` renamed and its `RUNPATH` and needed name edited, so name, digest and linkage all differ | refused, for the reason above, not because it looks like a compiler | refused, for the reason above |
| an **explicit dynamic loader**: `ld-linux-<arch>.so.2 ./renamed-driver` | refused: `mmap(PROT_EXEC)` of the job's file fails on the `noexec` mount | n/a (`dyld` does not load a standalone binary by argument); its equivalent is the next row |
| **library loading**: an approved process `dlopen`s, or runs with `LD_PRELOAD` or `LD_LIBRARY_PATH` naming, a job-written library or a renamed `librustc_driver` | refused: the mapping fails | refused: `file-map-executable` outside the allow-list |
| an **interpreter**: `python3 drive.py` where the script execs the pre-staged compiler; and `./drive.sh` with a `#!` line | the exec the script makes is refused as in row 1; `./drive.sh` is refused by name (a `noexec` mount); `sh drive.sh` reads the script as data and runs | the same |
| an **interpreter implementing the compiler**: a toy compiler written in the script | runs, **out of scope** and recorded: no exec, no foreign artifact | same |
| an **in-process compiler**: a test, a build script and a proc macro, each blaze-built, containing a toy Rust compiler that parses source and writes an object file | runs, **out of scope**: it is a blaze-built program, its provenance says so, and the fixture asserts the harness records "out of scope: in-process compilation" and does not count it as a boundary failure | same |
| **bytes brought in**: a non-host compiler's bytes in a fixture, loaded by the job's own in-process loader (anonymous memory, no file mapping, no exec) | runs, **out of scope**, recorded | same |
| **inherited descriptor**: the parent opens the host's rustc and starts the worker | the worker's `close_range` leaves no such descriptor; `fexecve` of its number fails with `EBADF` | the descriptor is closed by the same step; `file-map-executable` is denied if it is not |


*Positive fixtures of the dropped boundary, record:* the CLI's build script running `pnpm`;
`autocfg`, `version_check` and `rustversion` probes answered by blaze; a C dependency built with `cc`; a blaze-built test
program run through `CARGO_BIN_EXE_*` from the exec store; a blaze-built proc-macro library
mapped from it; a build script that compiles a C probe and runs it, and one that builds and
runs a `protoc`, each passing through the promotion path, which is a bug of the path if one does
not.

## 1. Identity

**Kept.** `StoreIdentity` and the entry key as they are. A function hit across machines needs
both machines on one identity; the target triple is in the entry key, so Linux and macOS entries
never meet.

**Added.**

- `CrateKey` (lookup), `JobContent`, `ExecutionId` and `Attempt` (section 5), `FrontEndKey`
  (which warm state can take a job), `TestBaseKey` and `TestKey` (section 6). Each is a digest of a
  canonical encoding, never of a path.
- **`ExecPlatform`** names the native side. It enters what is linked and run (programs,
  `TestKey`) and executor matching. Whether any of it changes a function's code (the macOS
  deployment target, the target CPU) is checked by unit C2's fixture; a field that does joins the
  entry settings of `CrateKey` (**unverified** today).
- **One blaze artifact per platform.** Every fleet machine and runner of one platform runs the
  same blaze binary, built once by a trusted builder from a pinned commit with `built_from` set,
  and fetched by digest. A published-crate install that falls back to hashing its executable
  (`ra-jit-store/src/lib.rs:133-157`) is not a fleet member. An upgrade is a new identity: nothing
  is shared with the old one, the cache is cold for it, and the old identity's records are
  collected (section 7). The server refuses a `Hello` whose identity it does not serve.

**What must become content-only first.** From #105 (**reported** there), each with the unit
that fixes it, before the unit that needs it:

| #105 item | Path-dependent today | Fixed by | Needed by |
|---|---|---|---|
| crate ids from absolute paths outside `CARGO_HOME` (`main.rs:938-951`, `:1676-1689`) | yes | C4 | C5 (cross-machine hits) |
| registry paths in stored code: panic locations, `file!()`, debug info (`$CARGO_HOME/registry/src/...`) | **unverified** whether a stored function carries machine A's path into B's program | C4 checks, and if so rebinds them symbolically in the stored value (a relocation the linker fills from the machine's paths) rather than normalising the key | C5 |
| build-script run keys (`cargo/script.rs` `run_key`) | hash the script's executable, its directory, arguments and variables, and what `OUT_DIR` held before the run (P0.5) | C4: hash the package's content, its dependencies' ids, the environment cargo gives and, on Linux, the digests of what the sandbox recorded it reading. Impure scripts are never shared | D2 (a helper not running every script) |
| proc-macro expansion keys (`ra-jit-expand`) | hold cwd and environment | C4: checkout-relative cwd, the environment cargo gives, recorded reads | D2 |
| per-target-dir test records (`tests-reach`, `tests-passed`, `CARGO_BIN_EXE_*` by size and mtime, `affected.rs:213-228`) | yes | T2 | T2 |

**Rejected.** A tree hash as the cache key (an edit anywhere would miss everything; research
7.1). The lock file in function keys (dependency crate ids already carry name, version and
metadata). Cross-host identities that let Linux compile for macOS (not known to give the same
code; no phase needs it).

## 2. Trust

### The access model (parity)

This is the whole of trust at parity: the model tracedecay runs today for NativeLink
(`docs/ci-remote-builds.md` at `d79de82273`, **read**), with `ra-jit serve` and the agents where
NativeLink's coordinator and workers are.

- **Reach is the tailnet's.** `ra-jit serve` and the agents listen on the tailnet address only, no
  public listener. Who may connect to what is a Tailscale grant by tag. The CI tag may reach only
  the server's API port on the coordinator host, plus what it reaches today; it gets no SSH and
  no agent registration. Fleet nodes carry their own tag and reach the server and one another.
  Write access (publish, submit a job, cancel one) is therefore limited to CI and fleet nodes by
  the ACL, as it is today, and blaze does not check it again.
- **Hosted runners join as they do today.** A GitHub-hosted runner joins the tailnet through the
  official Tailscale action with a GitHub OIDC identity that Tailscale validates (issuer
  `https://token.actions.githubusercontent.com`, `repository_id`, `actor_id`, `workflow_ref`,
  audience), restricted to the CI tag, as `TS_BAZEL_CLIENT_ID` is configured now. blaze does not
  read the token.
- **Fork pull requests are excluded.** The workflow does not give them the tailnet or fleet
  credentials, and GitHub's fork approval policy stays `all_external_contributors`; a fork run
  builds with no remote. Approving a fork run does not enroll its author. blaze needs nothing
  more to refuse them, since a fork cannot reach the server.
- **The principal is the tailnet peer.** A request carries no capability and no signature at
  parity, and the server serves one namespace (section "Scope"). Who ran a job is read from the
  job's own record and the tailnet's logs.
- **What this does not do,** as today: it does not keep code of a same-repository pull request,
  once the owner's workflow runs it on a fleet node, from writing the shared cache or reading
  what the worker's user can read. That is the accepted risk of the scope section.

### Out of scope (owner, 2026-10-26): isolation, capabilities, namespaces, signed records

Everything from "Threat model" to the end of this section, except the fork bullet marked below, is
the design of revisions 1 to 3.1 for the case where pull-request code on shared workers is
untrusted: separate `Main` and `Pr(n)` namespaces, a trusted-builder role that never runs pull-request
code, per-pull-request executors under their own OS user and sandbox, app-level capabilities
verified against GitHub's OIDC and REST API, server-signed records with epochs, and audit
rebuilds. The owner ruled it out as more than Bazel with NativeLink gave tracedecay. **No unit builds
it and no acceptance line depends on it;** it is kept as a record. Where later sections name
"`Main`", "`Pr(n)`", a capability, an epoch or a trusted builder, they name this record's terms;
the parity reading is given where it matters.

### Threat model

- **Trusted:** `master`'s commits after review, the fleet's operators, the blaze binary of
  section 1, GitHub's OIDC issuer and API.
- **Untrusted:** everything a pull request brings: its source, build scripts, proc macros, tests,
  and any process they start. A same-repository pull request is still untrusted: an account can be
  compromised and a dependency bump can bring code nobody read.
- **Goal:** nothing a pull request runs can change what `Main` holds, read a credential that
  writes `Main`, or make a later `Main` build run its code. A pull request may poison only its own
  namespace, which only it reads.
- **Out of scope:** a kernel escape on a shared host (section "Isolation" names the stronger
  option), and a compromised trusted builder (its output is what `Main` is; audit rebuilds in
  section 3 detect a divergent one).

Revision 1 isolated namespaces on the server but let one slot run a pull request and later build
`master` with the same daemon, store and user. Both reviews showed a poisoned local entry would
then be published under `Main`'s signature (review A B2, review B B1). NativeLink has the same
mixing for tracedecay today (research 7.8). Signing proves who published, not what ran.

### Roles

| Role | Runs | Writes | Holds | Lifetime |
|---|---|---|---|---|
| **Trusted builder** | `master`'s verified commits only, never a pull request's | `Main` | a builder credential, under the control plane's user | long-lived; warm on `master` |
| **Pull-request executor** | one pull request's pushes, as a lead or a helper | `Pr(n)` only | nothing; the agent publishes for it | created from trusted read-only inputs on the pull request's first job; kept for its later pushes; destroyed when the pull request closes, after 24 h idle, or on any sign of tampering |
| **Worker agent** (control plane) | no build; starts and stops executors, uploads their outboxes, reports | what a job's capability allows | its enrolment credential, mode 0600, under a user no executor runs as | per machine |
| **Requester** | a hosted runner, a developer | nothing on `Main`; `Pr(n)` when it is that pull request's job | a GitHub OIDC token, then a capability | one job |
| **Server** | the cache and the coordinator | records it signs | the signing key | per fleet |

### Isolation

What must hold, on every platform:

1. A pull-request executor runs as a user of its own (one per executor), with its own worktree,
   target directory, store overlay and temporary directory, all deleted with it.
2. Its view of the trusted store and front-end image (section 4) is read-only and cannot be
   written by its user. It writes new entries to its own overlay.
3. It cannot read the agent's credential, the builder's credential, or another executor's files:
   they are not visible in its sandbox, and their modes deny its user.
4. Its processes are one process group in one cgroup (Linux) or one session under one user
   (macOS), killed as a whole at the end of the job, on cancellation and on the deadline.
5. A trusted builder never runs anything an executor wrote, never imports an executor's packs,
   and never takes a `Pr(n)` record.
6. Network: an executor reaches only the server, through the agent's proxy; nothing else.

How, by platform:

- **Linux:** blaze's sandbox (`sandbox.rs`: user, mount and network namespaces, a read-only host)
  is extended from one test to a whole job (unit E1): a fresh user, the trusted store and image
  bind-mounted read-only, the overlay and worktree writable, `/home` and the control plane's
  directories not mounted at all. Today the sandbox maps the current user's id
  (`sandbox.rs:444-446`) and hides nothing (review B B1); E1 changes both.
- **macOS:** there is no blaze sandbox (`sandbox.rs:109-114`). Unit E2 adds one: a dedicated
  unprivileged user per executor and a Seatbelt profile (`sandbox-exec`) that denies reads of the
  control plane's and other executors' paths, denies writes outside the executor's directories,
  and denies network but the agent's socket. Seatbelt is deprecated by Apple and still enforced
  (**read** for one narrow probe on macOS 26.6.2, section "Not checked"; the production profile is
  **unverified**). Until E2 lands, the Mac Studio runs trusted builders only, and a
  pull request's macOS leg runs on a hosted macOS runner (section 5).
- **Stronger, the owner's choice:** a microVM per executor restored from a trusted snapshot, as
  [ci-runner.md](ci-runner.md) designs with ix (Firecracker on Linux, Virtualization.framework on
  macOS). It adds a kernel boundary and a cold-start cost that was not measured here
  (**unverified**). The contract above does not change; only how rule 1 to 6 are enforced.

**Rejected:** forking a warm trusted daemon into a pull request's executor (a "zygote"). It would
give a warm start for free, but blaze's daemon is multithreaded with a global thread pool, a
fork keeps only the forking thread, and macOS forbids fork without exec for processes that use
system frameworks (**unverified** for blaze). The image of section 4 gives the same sharing by
mapping files, with neither problem.

### From a GitHub identity to a capability

The server verifies a GitHub OIDC token (issuer `https://token.actions.githubusercontent.com`,
audience `blaze-serve`, signature, `exp`, `nbf`, `iat`, a `jti` not seen before) and reads only
claims GitHub documents: `repository_id`, `actor_id`, `event_name`, `ref`, `sha`, `workflow_ref`,
`run_id`, `head_ref`, `base_ref` (review B M3 lists these from the issuer's
`openid-configuration`, **reported**). It then asks GitHub's REST API, with the server's own
read-only token, what the claims do not say (the fields below are GitHub's documented ones; not
exercised here, **unverified**):

| Token says | Server asks | Grant |
|---|---|---|
| `event_name` `push`, `ref` `refs/heads/master`, `workflow_ref` = `<repo>/.github/workflows/ci.yml@refs/heads/master`, `repository_id` 1261602522 | `GET /repos/{repo}/commits/{sha}` and that `sha` is on `master` (`GET /repos/{repo}/compare/{sha}...master`, status `identical` or `ahead`) | requester on `Main`: lookup and fetch; a job for `sha`, which only a trusted builder may run |
| `event_name` `pull_request`, same repository and workflow file, `run_id` R | `GET /repos/{repo}/actions/runs/{R}`: its `pull_requests[0].number` n and `head_sha` H; `GET /repos/{repo}/pulls/{n}`: `head.repo.id` equals `repository_id` (same repository); `GET /repos/{repo}/commits/{sha}`: the token's `sha` is a merge whose parents are a commit of the base branch and H | requester on `Pr(n)` at that commit: lookup and fetch in `Main` and `Pr(n)`, a job for it, publish to `Pr(n)` through the job |
| anything else | nothing | refused |

- **Fork pull requests** get no tailnet and do not call the server, as today, and run blaze
  hosted-only with no remote; a call would be refused anyway, as the head repository differs. That
  is one policy; revision 1's "forks get read-only claims" is withdrawn (review B M3). **In
  scope:** this is the access model's fork rule above, and needs no server check at parity.
- **Capabilities** are `Capability` values signed by the server, valid for one execution and
  attempt, at most until the job's deadline, for the listed operations. The agent receives the
  executor's capability and uses it outside the sandbox; the executor never sees it. A trusted
  builder's capability names `Main` and the verified `master` commit; the server refuses a `Main`
  publication whose record `source` is not that commit.
- **Roles are separate credentials.** Enrolment makes a worker agent; it can long-poll for work
  and post events for executions assigned to it, nothing else. Being enrolled confers no
  publication right; only a capability does. Cancel, roots and namespace deletion each need their
  own operation bit.

### Records, epochs and repair

- **Signed records.** The server signs `CrateRecord` fields 0 to 5 with its Ed25519 key; clients
  pin the key (`RA_JIT_REMOTE_TRUST`, `signed.rs`). The signed bytes are the six-field map of
  those fields, without field 6, under the `sig/CrateRecord/v4` domain the schema gives every
  signature; a client checks them the same way. The namespace is in the signed bytes, so a
  `Pr` record is not a `Main` record.
- **The epoch** (REAPI's `salt`, research 7.8). Each namespace has a current epoch, answered in
  `Hello`. A client refuses a record whose epoch is not the current one. Raising `Main`'s epoch
  disowns everything in it at once, without deleting a byte; trusted builders then republish.
- **A forced rebuild of one crate** (`skip_cache_lookup`): a trusted builder compiles one crate
  without lookups and publishes a new generation.
- **Audit rebuilds.** A second trusted builder recompiles a sampled crate from scratch each day
  and compares entry digests with `Main`'s. A difference is a determinism bug or a compromised
  builder, reported either way. Whether blaze's codegen gives byte-identical entries across
  machines is **unverified**; unit C5 checks it first.
- **What the server signs** is "a trusted builder at this verified commit published these
  packs". It authenticates the publisher, as review B says, and only trusted builders can be
  that publisher. A pull request's packs are never promoted into `Main`: after a merge, `master`'s
  build compiles them again.

**Rejected.** Signing keys on hosted runners (whoever reads the secret signs `Main`). Unsigned
records with network trust. Promoting `Pr(n)` packs into `Main` on merge. One NativeLink-style
instance for `master` and pull requests.

## 3. Distributed cache

### The server

**Chosen: `ra-jit serve`**, one process on Ubuntu-main, over the `Dir` place's layout plus the
tables above; it also hosts the coordinator. The `S3`, directory and `https` places stay for
everyone else. What it adds over a bucket, each from research section 7:

- **Batch lookup** of a build's `CrateKey`s in one request, answered with the latest record of
  each in the namespace the server serves (at parity one, `Main`; the record's `Pr(n)` before `Main`
  order is out of scope).
- **Completeness** (NativeLink's `completeness_checking`): a record is answered with only the
  packs the server holds; one that names a missing pack is answered without it and the pack's
  absence is logged.
- **An existence set** (`existence_cache`): `missing` is answered from memory.
- **Verification on write**: a blob is hashed before it is kept.
- **Success only**: only a job that finished publishes records; a failed job's packs are not
  named by any record and are collected.
- **One namespace, no capabilities and no signing at parity** (section 2's access model: the
  tailnet's ACL decides who may write), and **GC by live roots** (below). The server and the
  records are as unit C1 landed them; the server fills the namespace as `Main`, epoch 0, and leaves
  a record's signature unset.

**Rejected.** MinIO behind the `S3` place as the fleet's server (one GET per record, bucket
credentials on runners, policy that cannot see a GitHub identity, a collector someone has to
schedule). **REAPI to NativeLink during the migration** (research 7.9): no batch action-cache
read (a build's 500 crate records are 500 calls), no namespace grants, LRU eviction that cannot
run GC by roots, and `ra-jit serve` is needed as the coordinator anyway. REAPI stays #226's
optional cache-only edge, after unit C5, mapping records to action-cache entries; nothing blaze
compiles could use NativeLink's existing entries, which are rustc's.

### Lookup granularity

**Per crate.** A publish cuts packs by home crate and adds one `CrateRecord` generation per
`CrateKey`; a build looks up its crates in one batch and fetches only the packs it lacks, before
the build. Per function is 165,752 lookups on nushell, 18.6 s serially at gRPC's 112 us unary
median ([ci-runner.md](ci-runner.md), #226; the median **reported**). Per project manifest
fetches every pack any build pushed.

A dependency crate with many generic instances (an `alloc` in every program's types) gets
entries from many builds. A generation is cumulative: it lists the packs of the build that
published it plus the packs of the generation before that the build used. So a newest generation
can still name packs whose entries no live build uses, and a pack is kept whole while any of its
entries is used. Generations alone cannot say what is dead (re-review A, b-M9); the GC below
collects by the entries builds used.

### GC

- **What is live is what builds used.** A finished build's `RootManifest` lists, per crate, the
  generation it read or wrote and a blob of the entry keys it used of that crate (the keys the
  lead's assembly read from the store or wrote to it; a key list per crate, sorted). An entry is
  live when the `used` blob of a live root names it.
- **Live roots, by configuration, not by commit** (re-review A N1): roots are grouped by
  `ConfigKey`, the `BuildSpec` without its source and lock, so one configuration's builds across
  commits are one history. Live: the roots of the last 20 `master` commits of each `ConfigKey`;
  every root written in the last 14 days, a pull request's included (at parity there is one
  namespace, so a pull request's roots are not collected apart).
- **Compaction makes generations smaller.** When the live entries of a crate's newest generation
  are under half of its packs' bytes, the collector writes exactly the crate's live entries (the
  union of the live roots' `used` lists for it) into fresh packs and publishes generation g' with
  only those packs. In the same step it appends `remap/<ns>/<crate_key>`: every generation g it
  replaces maps to g'. A root that names g is then served g' (which holds every entry the root
  used); g's record and every pack no live generation names are dead.
- **Leases and pins:** a running job's lookups pin the generations they answered, and an
  execution pins every blob its executors upload under its execution id, from the upload to the
  execution's end (section 4), until the job ends or its deadline passes; GC skips pinned
  generations and blobs, and a remap does not unpin. No eviction retry is needed (research, the adopt table's row 21).
- **Budget:** `--max` bytes with a low-water mark at 80%. Past `--max`, the server collects
  dead data, then roots outside the last 20 `master` commits of their `ConfigKey`, least recently
  written first; it never collects a pinned generation or pack. If live
  data alone passes `--max`, the server says so and stops accepting publishes before it evicts a
  `master` root.
- **Scheduling:** collection runs in the server every hour and when a publish crosses the budget,
  not by hand. A blob no generation names and no execution pins is removed after at least two
  hours (`collect.rs`'s grace) after its last pin ended, checked again right before removal.
- A pull request's roots expire 14 days after their last write; the per-namespace delete of the
  record's `Pr(n)` (`DELETE /v1/ns/pr/<n>`) is out of scope.
- Records of an identity the server no longer serves are dead after 7 days.

## 4. Distributed compile

### What it is

One build's compile work, split across executors on several machines, with the results joined
into one program. Not a job moved whole to a fleet machine: that remains how a job runs
("warm remote jobs", below), and is not the deliverable. Every build blaze runs in the fleet may
use it; the gate of this section decides how it is done, never whether.

There is no prior art to copy. Bazel, NativeLink, sccache-dist and buck2 distribute whole rustc
invocations, one crate each; none splits a crate (research 6, 8). The `tracedecay` crate is one
200.846 s rustc action on one worker today. blaze has no per-crate action to distribute either:
a crate's functions are queries over one database.

### The unit: the build's own ahead loop, in batches

A whole build compiles ahead in `ObjectCodegen::compile_ahead` (`mod.rs:5479`): threads take a
root or an instance from one queue, compile it, and push the instances its code names. LOADMEM
shows that a tracedecay build spends its time and its memory there, and u7 bounds it: the loop
takes its work in batches, and at the end of each it hands what it compiled on and releases the
batch's memos. That batch is the unit of distribution. A batch ends at one boundary, where its
functions leave the salsa memos: into the object in a local build, as linkable store entries in a
distributed one. Memory and distribution are one mechanism.

A batch is thousands of functions: coarse enough that a round trip costs nothing beside it (about
2 ms of thread time a function on nushell, roadmap.md, n not stated), fine enough to balance.

| Unit | Verdict | Why |
|---|---|---|
| one function per request | rejected | a request and its round trip per 2 ms of work |
| one crate per action (Bazel's) | rejected | blaze has no per-crate action: a crate's functions are queries over one database |
| static shards by stable-name hash (revision 1's mode D) | rejected | every shard needs the whole front end and, in blaze, the whole program compiled to find its share (below); a slow shard holds the build |
| **batches of the build's ahead loop, granted from one work set** | **chosen** | the loop u7 bounds, across processes; dynamic, so it balances |
| one big box | kept where it exists | Ubuntu-main is one (128 cores); several executors on it splitting one build are the first width GD measures |

### Moving work between processes: the decision (re-review B, B2)

A queue item is `(Instance, StoredParamEnvAndCrate)`, ids of one database, and a stable name cannot
be turned back into one. So the question is how an executor comes to hold an instance to compile.

| Option | What crosses the wire | Cost | Verdict |
|---|---|---|---|
| **(a) a lossless instance descriptor**: a versioned encoding in a new crate under `vendor/rust-analyzer` of source, unit, item, closure and coroutine identities, generic and const arguments encoded recursively, the asking param environment's caller bounds and the overflow mode, with rebinding into a helper's database under the exact `BuildSpec`; a type spelled past 1,024 bytes travels as a content-addressed blob, never as its hashed name | a descriptor per work item | an encoding of every type-system construct blaze compiles, kept in step with the front end; a wrong rebinding miscompiles silently unless entry digests are compared; round-trip and compile tests across interning orders, long nested types, closures, coroutines, const generics, caller bounds and repeated units. It is what would let a helper hold less than the whole front end, and re-review B's M2 limits that: an instance homed in a workspace crate needs that crate's closure, often most of the program, and how much of tracedecay's work homes in small closures is not measured | **second stage** (unit D2), built only if D4 names helpers' front ends as the limiter and D2's home inventory shows useful work that fits smaller helpers |
| (b) static shards over identical front ends: every executor computes the whole work set in one canonical order and compiles shard i of N | names, for coverage | every executor holds the whole front end (G0: 120 to 210 s cold; LOADMEM: 11.0 to 13.1 GB live at the resolution peak; warm executors amortise both). And in blaze an instance is found only by compiling its caller (`requests.funcs` comes out of codegen), so to know its shard each executor would compile the whole program | rejected as stated |
| **(b′) claimed discovery over identical front ends** | a root-set digest (with an index digest and an inputs digest), and for each found function the digest of its `WorkKey` and of the request behind it | the whole front end per executor, as (b); a total work key; a canonical root order that does not depend on interning; a pre-index of every function a grant can name; retained findings with a stated lifetime; a test that two processes name everything alike; one claim per found function, carried in batch results | **chosen, first stage** (units D0, D1) |

How (b′) works. Every executor of an execution loads the same `BuildSpec` and source and computes
three things from its front end.

- **The roots, as total keys (c3-B2.1).** Each root is a `WorkKey::Item`: the unit it is compiled
  in, the digest of its stable name, and a fingerprint of the asking param environment (the zero
  digest where the semantic layer proves the environment irrelevant), together with a `tuple`
  digest of the complete local request behind it. The stable name alone is not a work identity: it
  names the instance, its arguments and `checked` and not the environment (`stable.rs:228-260`),
  gives no name to some const arguments (`:597-607`), and the listing keeps several functions,
  of several crate builds, under one spelling (`:289-354`). Sorted by the bytes of the pair
  (`WorkName`, `tuple`), the roots are one list on every executor, whatever order its interner
  gave them. Two roots with the same stable name in different units, or asked under different
  environments, have different names. Two with the same `WorkKey` and different `tuple`s are
  distinct and sort by `tuple`, so an executor holding a tied pair swapped posts a different
  `RootSet`. Roots are deduplicated only when name and `tuple` are both equal, as the one-process
  build deduplicates the full request (`mod.rs:5500-5511`).
- **The pre-index (B2 refinement).** At join the executor builds a table from `WorkName` to local
  function for every function a grant can name without a caller's help: all eligible non-generic
  entries and all on-demand functions (`stable.rs:291` finds them by spelling), under the pinned
  `BuildSpec`, ties between units resolved by the key above. It is built from the stable listing
  the daemon keeps for the store (nushell: listing 4.7 s, n not stated) and posted as a digest.
  So an on-demand function is granted by `WorkName` to any executor, and the holder resolves it
  through the table; the full stable name never needs to cross the wire (sending it would also be
  no instance, but it makes every grant longer and the table makes it unnecessary).
- **The inputs (c3-B2.3).** A digest of the content digests of every build-script output and
  proc-macro library the front end consumed. A script or macro that is impure (section 1) runs
  once, on the lead, and its output is published as a blob; a helper uses that output by digest
  and does not run it again.

An instance is compiled only by an executor that found it. When a function the executor compiled
names a generic instance, the executor holds the instance's local request (retained, below),
reports `Found { name, tuple }`, and the coordinator grants the name to the first executor that
reported it and records the others, whose `tuple` equals the first's, as finders. A request with
no stable name cannot be reported by name: it is listed in the item's `unnamed`, the coordinator
records a terminal `WorkKey::Edge { parent, ordinal }` for it, and the item is `Local` (its entry
could not name the callee). The lead rediscovers the request by compiling that Local parent, in its
tail; nothing needs claiming first, so an unnameable discovery cannot be lost (c3-B2.1).

**Same name, different request (c3-B2.1).** Two finders that report one `WorkName` with
different `tuple`s hold requests the key does not tell apart. The coordinator does not pick one,
and does not merge them: merging requests by name before compiling would suppress the second
compilation that could show they differ. The name becomes a `Conflict`, which is terminal
`Local { identity conflict }`: no grant is made, none of its finders compiles it, and the
exported callers that name it are not adoptable (the closure rule, below). If a winner had
already exported an entry under the name, that entry is export-invalidated and the lead bans the
name, so the entry is never read from an imported pack (below). The lead compiles it, and the
conflict is counted as a determinism bug with both `tuple`s. Whether equal `tuple`s always mean equal code is not
assumed: D0's verification arm compiles the colliding cases independently and compares
(acceptance below). The same distinction applies to store keys: an entry kept under a stable
name alone is the one-process build's key (cache.md); C5 checks that the entry key tells apart
what `WorkKey` does, and where it does not and equivalence is not proved, adds the unit and
environment to it.

The price is the front end on every executor: hence warm executors (section 2's trusted builders
on `master`, a pull request's executors across its pushes) and D3's image. What it buys: no
instance transport in the first stage, D2's descriptor only if measurement asks for it, and warm
helpers that start before a cold lead is loaded.

**Finder retention (c3-B2.2).** The scheme needs each finder to keep, after its batch ends and
its memos are released, the owned local request of every name it reported: the instance and the
`StoredParamEnvAndCrate`, whose generic arguments and clauses are owning interned slices that the
type GC marks and traverses, not raw type values, which the GC's precondition forbids holding
unrecorded (`collect_ty_garbage`). The contract is:

- **Lifetime.** A finding is kept from the batch that found it until the coordinator reports the
  name terminal or releases the execution, or until the executor reports it `Lost`. Memo eviction,
  the type GC and the allocator's purge between batches run with findings held, and never
  destroy them.
- **Identity.** The executor's source and database identity (the salsa revision, with the body,
  closure and coroutine ids the findings point into) is pinned to the execution. A source change
  is a new execution, not a reload under findings held.
- **Lease.** A finder is eligible for a name only while it holds its finding. The coordinator
  records alternate finders (equal `tuple`) as well as the winner, because the recovery rows
  below grant a name to one of them. A granted name the holder cannot resolve through its findings
  or its pre-index is answered `Lost { lost-finding }` (or `unknown-name`, which is also counted
  as a conflict), and that holder stops being a finder of it: the name goes to another finder, or
  becomes terminal `Local` for the lead's tail. A missing name is never silently dropped.
- **Cost.** The registry's size (names and request sizes) is part of the executor's reservation
  (section 5) and of D0's recorded peak.

### The protocol

Roles in one execution: the **lead** (the executor that owns the job: it assembles the program,
links it and runs the tests) and **helpers** (executors of the same trust, platform and
`FrontEndKey`). Every executor, the lead too, compiles batches; the coordinator holds the work set,
a sans-IO state machine in `crates/blaze-dist`.

1. **Join.** An executor fetches the packs the execution's cache lookup answered (pinned to the
   execution), loads its front end, and posts `POST /v1/work/join {0: execution,
   1: attempt, 2: RootSet, 3: rejected}`. `rejected` is the names the lead's daemon already
   rejects (`Rejections`); every executor skips them, as `compile_ahead` does. No executor waits
   for the lead to seed: a warm helper joins and compiles while a cold lead is still loading
   (re-review A N6, re-review B M4). **What a join establishes (c3-B2.3).** Three different
   agreements are named here, and only the first two are a join's to check:
   1. *Root-list equality*: `spec`, `roots`, `count` and `index` equal.
   2. *Semantic-input agreement*: `inputs` equal, so the executors consumed the same expansion
      and build-script artifacts, not merely the same nominal `BuildSpec`.
   3. *Accepted-entry equivalence*: an entry a helper exports is the entry the lead would
      compile. No join proves it: equal sorted roots do not prove equal expanded bodies, request
      edges or param environments (an impure macro can change a body without adding or removing a
      root). It is checked by D0's verification arm on fixtures, by the closure check and the lead's
      import, and in production by section 3's audit rebuilds. A `RootSet` is an admission check
      and not a proof that the front ends agree.

   **Who is authoritative.** The lead's `RootSet` is. A helper that joins first is provisional: it
   is granted ranges and compiles speculatively. When the lead joins, a provisional `RootSet` equal
   to the lead's becomes the execution's; one that differs is fenced: its leases die, its
   uncommitted results are refused, its packs stay unnamed, and the difference is counted as a
   determinism bug with both digests. If no helper agrees, the execution restarts with the lead's
   inputs alone, serially, a one-executor build, and says so. The only executor the execution
   cannot do without is never refused. After the lead has joined, a join with another `RootSet`
   is refused (conflict: "the executor's front end differs") and counted.
2. **Grant.** An executor asks `POST /v1/work/lease` and gets a `Grant`: root ranges and the names
   granted to it, under a `Lease` with a new `generation`. Names come before roots, so a finder
   compiles what it found while its database holds it. A new generation for a holder is issued
   only after the old lease's remainder is accounted for: the batch's `BatchResult` returns every
   range and name it did not process (`returned_roots`, `returned_names`) in the same transition
   that commits its items, and the coordinator puts them back in the pool before replacing the
   generation. Work stopped at u7's budget is therefore never silently invalidated by the next
   grant (c3-B2.2).
3. **Compile a batch.** The executor runs its `compile_ahead` threads over the grant, as the local
   build does (u7's batch, the same code), with two differences: an instance its code names is not
   pushed onto the local queue but held until the name is granted, and a function the store has
   with its whole usable closure (`Entries::usable`, debug type sets included) in the execution's
   pinned packs is `FromStore`, neither compiled nor followed. A function that fails is `Failed`,
   with its error; a failure here is speculative, as in the local loop.
4. **Export, upload, report.** At the batch's end (its grant done, or u7's budget reached) the
   executor runs the exporter over what the batch compiled (below), finishes the sink (below),
   seals the entries into a pack, uploads it (`PUT /v1/blob`, pinned to the execution from the
   upload), and only then posts its `BatchResult`. Then it releases the batch's memos, holding its
   findings.
5. **Commit.** The coordinator commits a `BatchResult` in one transition: each item's outcome; each
   item's `found` names (a name not seen before is granted to this holder, a name seen before adds
   this holder as a finder if its `tuple` is equal and makes the name a `Conflict` if it is not);
   each item's `unnamed` requests as terminal `Edge` keys; the packs; and the returned ranges and
   names. Found names are registered in the transition that makes their finder terminal, so
   coverage never counts an item done before what it found is known (re-review B, M1). A replay of
   the same `seq` gets the same answer.
6. **Done.** The compile phase is done when every root range, every granted name and every `Edge`
   is terminal and no lease holds work. The root list's length is fixed when the lead's `RootSet`
   is, so an empty work set before the roots are in cannot signal done.
7. **Assemble.** The lead imports every accepted pack, synchronously, and builds the program as a
   store-hydrated full build (below), compiling only what nobody could export.
8. **Outcomes are not verdicts.** No outcome of a batch fails the job. A `Failed` function matters
   only if the program needs it: the lead's assembly then compiles it, gets the same error, and
   goes through the rejection rounds of a one-executor build (`mod.rs:636-645`; `whole.rs:120-135`
   fails on a stand-in the program reaches), with no rustc. So an unused function blaze cannot
   compile fails neither build (re-review B, M7). A `Local` item, and every exported item whose
   usable closure reaches it, is compiled by the lead: the lead's tail, counted by reason.
9. **Leases and fencing** (re-review B, M3; re-review A N5). **Compute is at least once after a
   holder dies; terminal accounting is exactly once.** With no fault and no verification arm,
   the coordinator's one owner per work key gives one compile per key. After a holder is lost, a
   lease is revoked or a name goes to the lead's tail, another executor or the lead may compile the
   function again, and neither fencing nor name grants can prevent it: a dead holder may have
   compiled and uploaded before it died. So no gate asks for exactly-once compute after a fault;
   duplicate compiles are reported as a diagnostic (D0, D4), and the verification arm duplicates
   deliberately. Exactly-once is of the accounting: each work key ends terminal once, from
   one holder's committed result.

   | Event | Transition |
   |---|---|
   | a heartbeat (every 5 s, on `/v1/workers/events`) | the holder's lease end moves to now + 60 s |
   | no batch committed within the progress timeout though heartbeats arrive: 300 s from the grant until the execution has a median batch time, then 10 times the median, at least 120 s (starting values, replaced by D1's measurement, **unverified**) | the lease is revoked, as for a lost helper (a hung helper) |
   | a helper lost: its lease end passed, or revoked | the lease's generation is dead; its open root ranges return to the pool; each name it held goes to another finder of it that still holds its finding, else becomes terminal `Local` for the lead's tail; a root or name whose lease has been revoked 3 times goes to the lead's tail too |
   | a `BatchResult` whose `Lease.generation` is not the holder's current one | refused whole (conflict), `found` and `unnamed` included, so a root-only result from a dead generation registers no names; its packs stay unnamed and are collected after the execution; the items are another holder's or the lead's |
   | a result committed whose answer was lost | the holder replays the same `seq` and gets the same answer |
   | an upload that fails | the holder retries it under its lease; if the lease ends first, the batch's items go back with it |
   | the lead's import of a pack fails | that pack's items join the lead's tail, named in the report |
   | the lead lost | the execution's `Attempt` goes up; every lease of the old attempt is dead and its late results are refused; every outstanding root returns to the pool; every outstanding name goes to a finder of the new attempt that still holds its finding, else to the new lead's tail, where the lead gets the request by its normal required-reachability assembly, never by decoding a digest; findings registered by the dead attempt's executors are invalid; outcomes committed under the old attempt stay (their entries are content, their packs pinned to the execution) and the new lead imports them |
   | the deadline, or no compatible lead | the execution ends `Cancelled` (deadline) or `Failed` (no lead after the reassignment of "Warm remote jobs"), named; the agents kill each executor's process group (section 2, rule 4); the coordinator drops the work set; the pins end; committed entries stay content, and no coverage is claimed |

10. **Liveness (c3-B2.3).** No executor waits on another to compile. The property is: **every
    finite execution ends, with `Done` and required-work coverage complete, or with a named
    `Failed` or `Cancelled`**. A lease always ends, by a result, by a lost heartbeat, by the
    progress timeout (which runs from the grant, before any median exists) or by the deadline;
    the work of an ended lease goes back to the pool, to a finder or to the lead, with a retry
    bound after which the lead takes it. The deadline is an end, not a `Done`: an execution that
    reaches it did not complete its coverage, and says so. Helper reservation is non-blocking: a
    lead that finds no helper free compiles alone, and two leads never hold helpers waiting for each
    other.

### The protocol in `crates/blaze-dist`

The work set, an executor's side of it, executions and admission are sans-IO state machines in
`crates/blaze-dist` (unit dsm). Each call takes `now` and a `blaze-wire` request, answers with a
`blaze-wire` answer or a refusal, and appends what it did to a log; nothing in the crate does IO,
starts a thread or reads a clock. D0 drives them in the lead's process, D1 over HTTP, E3 and E4 in
`ra-jit serve`. Where each rule of this section and section 5 lives:

| Rule | Where |
|---|---|
| Join: the lead's `RootSet` authoritative, a first helper's provisional, a differing one fenced and refused, the work set restarted on the lead's list (step 1, c3-B2.3) | `work::WorkSet::join`, `join_lead`, `reset` |
| Grant: names before roots, the first finder first; a holder's live lease asked for again is the same grant (step 2) | `WorkSet::lease`, `grant` |
| Commit in one transition: outcomes, found names with their finder, conflicts, `Edge`s, packs, returned work back in the pool before the next generation, a replay answered as before (step 5, c3-B2.1) | `WorkSet::result`, `commit_name`, `register`, `shape` |
| Done once the lead's root set is fixed, every root, name and edge terminal and no lease live (step 6) | `WorkSet::check_done` |
| The fencing table: heartbeats, the progress timeout, a lost helper, a dead generation, the lead lost, the deadline (step 9, rb-M3) | `WorkSet::heartbeat`, `tick`, `lose`, `fence`, `new_attempt`, `end` |
| Finder retention and `Lost` answers (c3-B2.2); an unknown name a conflict and `Local` (c3-B2 pre-index) | `holder::Holder::plan`, `committed`; `WorkSet::commit_name` |
| A lead that misses three heartbeats, one reassignment, a dead attempt's calls refused, a second loss `Lost` ("Warm remote jobs") | `jobs::Execution::tick`, `assign`, `worker_events` |
| One execution per `JobContent`, subscribers' own deadlines and policies, cancellation when the last leaves, a pull request's new push (E3, a-M6) | `jobs::Jobs::submit`, `Execution::unsubscribe`, `tick`, `jobs::local_action` |
| Capacity and reservations, `Main` first, `Q`, platform matching, rebuild by drain (section 5, E4) | `admission::Admission` |

What the crate fixes that this section leaves open, one line each:

- A batch's items are its processed roots' in ascending index order, then its processed names'
  in the grant's set order, one each. A result of another shape is refused `malformed` and its
  lease revoked.
- A holder's lease ends with the holder: 60 s with nothing heard from it (a heartbeat or any
  work call) drops it and its findings, and it joins again as new. A lease revoked by the progress
  timeout drops its holder the same way.
- `Lost { lost-finding }` sends the name to another finder; `Lost { unknown-name }`, and a `Lost`
  with any other reason, makes it terminal `Local` and counts a conflict.
- An item reported `Exported` or `FromStore` with an `unnamed` request is committed `Local`.
- A join with the same root set, and a lease asked for while one is live, are answered as before,
  so a repeated message changes nothing.
- Before the lead has joined, a helper whose root set differs from the provisional one is refused
  `not-yet` with a retry time; after, `front-end-differs` for the attempt. A new lead whose root
  set differs from the fixed one, after a reassignment, is authoritative too.
- A lead that is loading its front end heartbeats; only a lead that stops heartbeating is lost.
- A stop effect and its acknowledgment carry the execution and attempt that occupied the
  executor slot. Only a matching pending stop releases that slot; a delayed duplicate cannot
  release a newer assignment or finish its drain.
- Worker events expire subscribers against their own deadlines before accepting a terminal
  result, just as a timer does. Each subscriber receives its outcome once.
- Worker ingress validates a whole batch before changing the stream, refusal or liveness.
  Queued, Admitted, Assigned, Compiled and Done stages and `Assigned` events are coordinator-only.
  A lead may report forward Fetched/Loaded/Published/Linked/Tested/Reported progress; stage
  timestamps use coordinator time and duplicate latest stages do not append again. Post-compile
  progress requires current authoritative completion, or explicit `NoCompileWork` admission.
  A lead may also report `Published`, `Program`, `Verdict` and one final `Done` (Passed, Failed
  or Refused), last in its batch. Both lead and helper may report `Output` and `Refused`;
  helper stages and job verdicts, backwards progress, reserved stages and duplicate Done are
  refused. Accepted stage updates always go through the coordinator's stage bookkeeping.
- `ExecutionConfig::completion` defaults to `WorkProtocol`: Passed requires completed work,
  the current lead's agreement, and the current attempt/root-epoch completion notification.
  Retained work does not bypass a replacement lead's agreement. Admission may explicitly
  choose `NoCompileWork` for verified cache-only artifacts or an already-built test shard;
  that mode rejects work calls. Empty BuildSpec or a worker Stage is not that decision.
- Refusals keep section 8's codes, and each message starts with a stable token (`dead-lease`,
  `attempt-moved`, `not-joined`, `not-yet`, ...) that an executor reads back.
- Admission is strict per platform: the head of `Main`'s queue first, a pull request's job only
  while no `Main` job waits, and a head that cannot run blocks its class. An idle executor takes a
  job only if its reservation covers the job's measured need.

The permanent-stall regression keeps a promoted lead heartbeating without completing its
batch. It checks deadline cancellation, no false coverage, fenced late results and stop effects.
It does not establish successful recovery from a heartbeating stuck lead. These state machines
and synthetic simulations do not certify compiler, transport, publication or process cleanup
integration; their effects still need drivers.

**The simulation.** `blaze_dist::sim` runs the machines on one thread over a seeded scheduler and a
virtual clock, as roadmap.md's simulation backend describes. The work simulation puts the
coordinator (inside an `Execution`) and two to four executors over a network that, until a fault
horizon, loses up to 25% of messages, duplicates up to 15%, delays them up to 3 s (and so reorders
them) and partitions executors; executors die before or after any effect, clocks run at 0.8 to
1.25 times (one at 0.05 to 0.2), one helper may have another front end (a swapped tie, another
root, impure inputs), one may stall past the progress timeout but eventually finishes, findings
are dropped and call sites disagree on tuples. One
executor never dies. The checker holds, from the coordinator's log and the calls it sees: no root
or name in two live leases; no result committed for a fenced or expired lease (expiry by the
checker's own clock), of another attempt, or after Done; at Done every root, found name and edge
terminal, from exactly one accepted result whose item is the lead's front end's, or in the lead's
tail; an unknown name a conflict or `Local`; Done reached unless the lead was lost twice or no
lead was left; a correct holder never told it is malformed. The jobs simulation checks capacity,
`Main` first and first in first out, `Q`, platform and namespace matching, assignment identity,
attempts, subscriber deadlines, typed terminal delivery, public-stream stage order and that every
execution ends. It injects forged worker stages and checks that rejected batches leave no prefix
in the public stream. These jobs explicitly model `NoCompileWork`; compile coverage remains the
work simulator's responsibility. Direct ingress tests cover both completion modes.
`cargo test -p blaze-dist` runs 10,000 seeds of each and prints the seed of a failure with the
command that replays it. The [review report](reports/DSM-REVIEW.md) records the regressions,
validation commands, timings, mutation checks and the original integration limits.

### Additive subscriber-terminal contract, version 2

`blaze-wire::job_terminal_v2` adds a separate typed terminal response. It does not add a field or
variant to `JobEvent` or edit `BuildSpec`. A v1 Done stream is retained for display and replay,
but is **not** enough to preserve the termination reason: Lost can be NoLead or LeadLostTwice.
NoLead is canonical Lost in both the v1 stream and subscriber delivery, whereas a reported
build/test failure is Failed.
The driver must return `Jobs::terminal(execution, subscriber)` verbatim, rather than synthesizing
a reason from Done. `Effect::SubscriberEnded` carries this same immutable response.

Proposed terminal route: `GET /v2/jobs/<execution>/subscribers/<subscriber>/terminal`.
The HTTP body (or one existing length-delimited Unix response body) is exactly one deterministic
CBOR map, content type `application/cbor`; no concatenated frames or JSON identities. A pending
subscription has no terminal yet. Retries return the same record. The route's terminal-contract
version is explicit in field 3's nested field 0, which must be 2.

```
SubscriberTerminalV2 = { 0: ExecutionId, 1: Attempt, 2: subscriber u64,
                        3: { 0: version u32, 1: Outcome, 2: TerminationReasonV2 } }
TerminationReasonV2 = [ discriminant, null ]
  0 Reported          -> Passed | Failed | Refused
  1 NoLead            -> Lost
  2 LeadLostTwice      -> Lost
  3 Cancelled          -> Cancelled
  4 Deadline           -> Cancelled
  5 SubscriberDeadline -> TimedOut
```

`decode_terminal` enforces negotiated `Limits.event` (1 MiB by default), normal schema depth
(32), string/collection limits and canonical CBOR, including no trailing frame, unknown field
or variant. Unsupported terminal versions and inconsistent pairs are rejected before policy.
`local_action` now requires `TerminationV2` and returns a typed refusal for invalid contracts.
Reported outcomes are verdicts under Never/Fallback/Race, including failed tests and refusals.
Infrastructure, cancellation and timeout fail under Never and choose local work under
Fallback/Race (Race's local arm is already running). A subscriber's own timeout remains distinct
from the execution deadline. Old clients must fail incompatibility, not guess a fallback policy
from a v1 outcome. The unpublished local 187e2a39 draft used Failed+NoLead; this is not an
accepted alias. A consumer of that draft needs its pinned codec and an explicit checked
conversion to canonical Lost+NoLead, not implicit dual decoding.

This terminal version is **separate** from the general wire schema and digest-domain version.
This lane inherits schema 1; C2's independent schema-2 branch adds required workspace/config
fields and changes typed digest/signature domains to `/v2`. Combining the branches must preserve
C2's global version/domains (including its schema-3 path work), merge the isolated module export, and update BuildSpec fixture
literals. New terminal type names and explicit terminal version remain; this addition makes no
promise that general schema-1 identities survive C2's change. Existing HTTP `/v1` routes and
request-auth framing have their own versions. A general schema-1 peer requires its pinned codec,
not a schema-2 decoder accepting old shapes.

The codec, effect-to-response model and client-policy function are implemented and tested.
The local Unix driver in [blaze-dist-local](../crates/blaze-dist-local/README.md) now implements
length-framed deterministic CBOR, explicit transport/global-codec/terminal negotiation,
daemon-issued session labels with deliberate release, actual registry routing and immutable
typed terminal/pending lookup. It retains exact request replies over reconnect, enforces
bounds and forwards real effects to the backend.
Its CLI and socket tests do not supply a compiler holder or execute a fallback/race build.
HTTP/fleet transport, source/store/compiler execution and production client-race behavior
remain D1/E3 integration work.

### Export and ingestion (re-review B, B3)

**What a batch end lacks today.** A batch ends holding `CompiledFunction`s, which are salsa memos
and not store entries. Three things are missing from that moment, and each has a unit that supplies
it (check 3, B3):

| Missing at a batch's end | Why an object-free helper cannot take it from the batch | Supplied by |
|---|---|---|
| the **final build's ownership and import policy**: which symbols are native, owned, shipped, TLS or foreign, which crates the build owns, the runtime shim policy | today `later.rs` classifies a callee by what the object around it has defined (`Later.ids`, import booleans, `later.rs:23-30,48-55`); a helper's batch has defined whatever callees it happened to compile, so its classification could differ from the lead's | D0: `ExportContext` below, built from the `BuildSpec` and posted as a digest |
| **request-level derived metadata**: `TypeId` hashes, `FunctionDebug` (variables, scopes, rows), patch tracking, vtable and drop-glue data | today each comes from an object-side map (`FuncId` to `TypeId` only after a shim is generated, `mod.rs:6902-6945`) or from a query over the request's environment and source | D0: the exporter derives each from the request itself |
| **durable writer and type-set completion**: every entry and every debug type blob it references written, flushed and sealed | the compiler's `cached::Writer` has its own queue and threads (`cached.rs:680-753`); the store's `settled` and `seal_open` do not wait for them | P0.3 supplies the sink seam at the batch end (its hand-on step takes a sink); D0 implements the sink's durable finish, below |

- **The exporter and its `ExportContext` (c3-B3.1).** The exporter (`hir-ty-codegen/export.rs`,
  new) writes the `cached::Entry` that `later.rs` writes today, from a `CompiledFunction`, its
  instance and home, without an object. It cannot be handed an object's already-resolved maps, so
  `ExportContext` resolves *every request*. It has two parts:
  - an **immutable per-build policy**, the same on every executor and the lead (its digest is
    part of `RootSet.inputs`): the complete unit and source identity; the root and on-demand
    registry (the pre-index of section 4); the actual native, owned and shipped link declarations
    (symbol names, TLS, the runtime shim policy); the target, backend, debug and key settings;
    and the own-crate set. It is derived from the `BuildSpec` and the shipped standard library,
    never from whichever callees a batch defined, so a helper and the lead classify one request
    alike.
  - a **pure per-request classification**, with no callee compiled and no object mutated: it
    returns `Named` or `Root` by stable name, `Import` only for a symbol the policy's link
    provides (an `extern` item, a symbol of the shipped standard library), `TypeId` by the hash of
    the type, `Asm` by its text, data by its blob and symbolic request-index relocations (C4's
    symbolic rebinding of paths), and `tracked` as `later.rs::tracked` makes it. It never calls
    `ObjectCodegen::resolve`, which emits shims and statics and schedules more code (`mod.rs:6236-6302`).
    The `TypeId` of a request comes from the request's type and instance, as `type_id.rs:132-170`
    hashes it, not from a `FuncId`. Debug and tracking need the preserved asking environment,
    source spans, rows and slots and the debug settings, kept until `FunctionDebug` and tracking
    are made; their recursive type sets are written before the batch's memos are released.
  - **Unknown is `Local`.** A request the classifier cannot place is `Local { why }`, never an
    assumed `Import`. `later.rs`'s `entry` and `keep` call the same classifier, answering from the
    object, so a local and a distributed entry come from one code path and an executor's entry
    digest equals the one-process build's (D0 checks it); the object-backed callers keep the
    mixed and rustc-copy behaviour they have today where it is needed, and a distributed no-rustc
    build does not enable it. No instance descriptor crosses the wire: every identity is consumed
    locally into the existing portable `Entry`.
- **The sink's durable finish, and seal and import as a barrier (c3-B3.3).** The sink's finish at a
  batch's end returns the final keys of every entry and type set it wrote, after it has
  flushed the compiler's `cached::Writer` (its queue and threads), settled the store, sealed the
  open pack and validated that the pack holds every key listed. The `BatchResult` is posted only
  after that. Seal is therefore a **writer barrier**: nothing of the batch is written after it.
  Import is a **cache generation boundary** on the lead: `Entries` memoises a missing read and a
  false `usable` (`cached.rs:801-852`), and `Store::refresh` only adds sealed pack indexes
  (`ra-jit-store/src/lib.rs:1457-1481`), so a lead that looked an entry up before import and found
  it absent would keep that answer. The lead's import installs all accepted packs at once,
  refreshes the store, and builds a **fresh `Entries` view and a fresh full `ObjectCodegen`** with
  no old patch base (`mod.rs:4980-4992` and `:5206-5212` make a new `Entries` and `roots_from_store`
  only when `base` is `None`), then records the store-backed base and its tracked marks.
- **The lead's assembly is a store-hydrated full build.** Today the checkout's code comes from the
  store only in a daemon's first full build (`hydrate`, `mod.rs:671`), and a patch neither compiles
  ahead nor keeps entries. A distributed execution's lead builds with `hydrate` and
  `roots_from_store` set whatever its daemon built before (a fresh object, as above), so every root
  and instance whose accepted closure is in the imported packs comes from the store; and it keeps
  the imported entries' `tracked`, so that its next edit can be a patch of this build
  (`patch.rs:1142-1181` keeps hydrated tracked generic code; `:1246-1272` reads stored root marks).
  A body edit stays a local patch (it compiles a few functions and leases nothing, by GD's
  batch-size rule). A signature edit or an identity change renames what is downstream; it is a
  distributed execution, and the lead assembles it hydrated again.
- **What is adoptable: raw, accepted and export-invalidated (c3-B3.2).** `Exported` is a raw
  outcome: a batch wrote the entry. Whether the lead can adopt it depends on its *closure*, and the
  existing `Entries::usable` is not that test: it walks `Named` callees only (`cached.rs:493-501`),
  while a `Root` edge means "want this root and import its symbol" at stitch time (`mod.rs:6033-6038`).
  So at Done the coordinator computes, from the results and the pinned packs, three sets:
  - **raw**: every entry any batch wrote;
  - **accepted**: a raw entry (or a `FromStore` one) every `Named` and `Root` edge of which reaches
    an accepted or `FromStore` entry, with its debug type sets present. This is a new
    closure test, `accepted_closure`, in `cached.rs`, not a call of `usable`;
  - **export-invalidated**: a raw entry whose closure reaches a `Local`, `Lost`, conflict or
    needed-`Failed` item, a missing entry or a cycle through one, with the reason and the item
    reached. An exportable parent of a `Local` generic lands here: the lead has no descriptor for
    the generic, so it rediscovers it by compiling the parent. The lead **bans** each
    export-invalidated name in its `Entries` (the banned set `Entries::get` already consults,
    `cached.rs`), so an entry of it that sits in an imported pack is never read, and its pack
    is not partly adopted by a side door.
  Local and missing outcomes thus propagate to ancestors before `Done`, Root edges included; only
  the accepted set is imported as reusable. (Partial adoption, with a way to recover a missing
  local instance without compiling its exported parent, would need a descriptor; it is D2's, if
  ever.)
- **Zero compile queries, with the counted set stated (c3-B3.2).** After import, the lead counts by
  stable name every run of `compiled_root` and `compiled_instance` (a query executed, not a memo
  hit), each with its reason. The lead may run a query for: a `Local` item; an
  export-invalidated name (named with the item it reaches); a `Failed` item the program needs; a
  conflict; a rejected function; or a lead-only function (statics, the allocator and runtime
  shims). "The store had it" is never a reason, and neither is "it was exported". **The number that
  must be 0 is the number of queries for names in the accepted set**, in the lead's assembly phase
  and, separately, in its granted phase, on a cold build and on a warm signature edit. It is
  reported next to the sizes of the raw, accepted and export-invalidated sets and each executor's
  accepted share: 0 is no evidence if the accepted set is empty or one executor's share is trivial,
  so GP's bar also requires both.

### What stays the lead's

Statics, the allocator and runtime shims, vtables and drop glue reached only through them, the
rejection rounds (`mod.rs:800-945`), `Local` items and what reaches them, and the link. The tail is
measured by reason in D0 and D4, not assumed small.

### The first unit: two processes on one Mac (gate GP)

Before any network code, unit D0 proves the mechanism on this Mac: a lead and one helper, two blaze
processes with their own stores and daemon homes, the coordinator in the lead's process speaking
section 8's work calls over a Unix socket, splitting one build. The calls are CBOR from
`blaze-wire` (C1) and the build is a `BuildSpec` from C2: D0 does not make a second
implementation of either. GP has two stages with different prerequisites and different
statuses (c3-D0.1):

| Stage | Where | Builds | Needs | Can be |
|---|---|---|---|---|
| **GP-fixtures** | Docker's Linux VM on this Mac until P0.6; the Mac itself after | `demo/mid` (regex-syntax, serde_json and memchr: generic-heavy), `demo/real`, the fixtures of D0's acceptance, with `BLAZE_WHOLE_BUILD=1` | P0.2, P0.3, C1, C2; P0.6 for the Mac arm; the preflight below | closed on Docker before P0.6 |
| **GP-full** | the Mac, native | `-p tracedecay --lib`, the crate that is one 200.846 s rustc action under Bazel | P0-H on macOS (so P0-E, P0-B and the rest of Phase 0), and the capacity preflight below | not closable before native P0-E and the capacity preflight; **pending** until then, never skipped and never reported passed |

D1 needs both closed, three recorded runs of each required arm and of the full stage, not a
successful demo-only invocation. This resolves D0's tracedecay stage: it is not an independent
early prototype, it is the part of the gate that needs P0-H, as every unit after Phase 0 whose
acceptance runs tracedecay does; D0 lands on GP-fixtures with GP-full recorded `pending` and
its prerequisites named (c3-D0 prerequisites).

**Two platform builds and a preflight (c3-D0.1).** A Mac `BIN` is a Mach-O arm64 binary and
cannot run in Docker. `proto.py --platform macos|linux-docker --stage fixtures|tracedecay|all`
names its binary: `BIN_MAC`, built natively as the phase plan's `BIN`, and `BIN_LINUX_AARCH64`,
built inside a pinned Docker image with its own target directory, with rust-src, the shipped
standard library and the native linker inputs in the image. Both builds run under the heavy-lock
wrapper, one at a time and never beside a tracedecay build. Mac blaze processes get the three
standing variables and one aggregate watchdog. Docker processes get an explicit cgroup budget
(`memory.max` per executor and for the coordinator and harness) set from the VM's `MemTotal`
that the preflight reads (8,317,267,968 bytes, 18 CPUs, Docker Desktop 29.9.2, aarch64: **read**
by u2-check3, one reading, no container started), not from a guess. "Linux, where the whole build
works" is broader than the evidence: Linux AArch64 has CI for Cranelift, LLVM and full link
(support.md), and `cargo blaze` there is unverified, so a whole-build success in Docker is
**unverified** until the preflight has passed. `proto.py --preflight` runs the exact path D0
needs, on the platform it names: a hello-world whole build with the standard library linked, a
two-process export and import of one function, and the spawn API's own probe (a refused compiler
spawn); it fails closed, naming what is missing (the pinned image, the standard library, the
native linker inputs, the cgroup budget), because Docker being installed proves none of them.

**Capacity of two front ends (c3-D0.2).** P0-C's bar is one worker at 16 GB footprint; two
residents of that size would need 32 GB before any child, so "wait for P0-C" is not a
sufficient prerequisite for a gate whose guard is 24 GB summed. LOADMEM's one profiled run
shows why it matters: 14,000 MB footprint at the peak of resolution and 13,054 MB after it
(**read**, one run, no spread; limiter: the retained front end and the allocator). A helper
that stays resident at 13.1 GB beside a lead at its 14.0 GB resolution peak sums to 27.1 GB.
That is illustrative capacity arithmetic, not a measurement of two processes, and no run has
measured `-p tracedecay --lib` as two. So GP-full starts with an **aggregate-capacity
preflight** for that exact `BuildSpec`: both executors to readiness (front end only), then
bounded batches, with their retained findings, export queues, store mappings and the linker and
test children, the harness, the coordinator and both executor trees under **one** watchdog and
one heavy-lock lifetime, every standing `RA_JIT_*` setting on, recording the simultaneous
aggregate footprint and the swap series at 1 s, not each process's peak. Staggering the helper
first is one tried mechanism. If the preflight fails, GP stays pending and the split is
implemented differently: a measured stronger P0.4 target, reclaiming or sharing front-end
state (D3's image), or another mechanism whose combined reservation fits. The guard is not
raised or lowered to make it fit, and a capacity failure is never recorded as a skip or a pass.

**GP's bar**, in every run of every required arm:

1. *Output.* The program's output equals a one-process build's (the demo programs' output; for
   tracedecay, the list of tests and the verdicts of a pure subset).
2. *An isolated, independent baseline (c3-D0.3).* The one-process baseline is built from a cold,
   empty private store, database and daemon home, with recomputation and encoding forced for
   every compared key, and archived immutably (entries, debug type sets, program bytes). No arm
   is seeded from another's store. Two store hits, or an export compared with itself, are not a
   differential check.
3. *Per-key equality.* Every entry key an executor exported equals the baseline's bytes, debug
   type-set closure included. A contested instance is compiled only by its winner in the sharing
   arm, so "every name's digest across executors" is checked in a separate **verification arm**,
   which compiles duplicates, the colliding unit and environment cases and a sample of the
   rest independently under different interning and resolution orders. Duplicates stay out of
   the sharing arm, which is the one whose shares and tail are reported.
4. *Counted from before load.* The per-name compile-query trace starts before the front end
   loads and covers every path that compiles: `lower_until`'s `compile_unnamed` (`main.rs:7904-7910`;
   `mod.rs:7092-7109`, `:2592-2601`), the whole build's root pre-pass (`mod.rs:872-881`) and
   `compile_ahead`. A distributed run forbids an ungranted compile query: those paths take their
   work from the grant source or do not run. Three numbers are reported apart: the lead's
   granted-phase queries, the helper's, and the assembly, rejection and base-record queries
   after import. The zero bars (c3-B3.2): assembly-phase queries for names in the accepted set are
   0, and the lead's queries, in either phase, for accepted names a *helper* exported are 0. Names
   the lead itself exported in its granted phase are its own share and are not counted against it;
   an export-invalidated name is compiled by the lead with its reason and is not counted against
   it either.
5. *Not vacuous.* The accepted set is nonempty and printed beside the raw and export-invalidated
   sets. The final stitched program's entries each carry an origin (lead-compiled, a helper's
   pack, the store), and the helper's share is nonempty and holds roots *and* discovered generic
   instances. Each executor's share is at least 5% of the accepted entries (a starting value,
   **unverified**, to be replaced by D0's measurement). A run in which the helper's results were
   refused, lost or unused and the lead compiled the rest fails with "serial fallback", whatever
   the output and digests say.
6. *No rustc.* 0 compiler execs started by blaze and a provenance record with no rustc-built
   artifact, in both trees (P0-A's rule on both processes).
7. *Reported.* The lead's tail by reason; each process's peak footprint and, on the Mac, the
   aggregate series.

Any wrong digest, coverage hole, lost finding, ungranted compile or masked serial fallback fails
GP. No network layer (D1) is built before both stages have passed.

**What a failed GP falsifies (c3-D0.4).** It falsifies this mechanism (claimed discovery, the
exporter and the ingestion) on the fixture that failed, not distributed compile as a deliverable:
the program's owner requires it, and a gate that fails changes how it is done. A failure blocks D1
and sends the work back to a corrected work key, exporter, recovery strategy or, if the
failure is that helpers cannot name what they find, D2's descriptor. A pass is not a proof for all
Rust, all platforms or all front-end states: a sample cannot settle that two front ends agree, so
the work key's contract is implemented with differential and fuzz coverage of interning and
resolution orders beyond D0's fixtures (D0 lands the first corpus). GP has no network and so
establishes no cross-machine speedup and makes no timing claim; D4 measures.

### Where each executor's front end comes from

| Option | Cost, measured | Verdict |
|---|---|---|
| an executor loads the whole workspace cold for one build | tracedecay on macOS: load 79.9 to 159.2 s, resolve 20.4 to 26.4 s, macros 19.85 to 23.73 s (2 runs, TRACEDECAY.md); 11.0 to 13.1 GB live at the resolution peak (2 runs, LOADMEM.md); 58.3 GB when ready with the whole build unbounded (1 run) | rejected as what a build pays: it costs more than the codegen it would split. D0 does it on fixtures, to prove the mechanism |
| **a warm full front end**: a trusted builder warm on `master`, a pull-request executor warm on its pull request's last push | an edit is 258 ms on nushell (roadmap.md, n not stated); tracedecay's edit time as the daemon's is unmeasured until P0-D | **chosen**, first stage: a job is an edit of what each executor loaded, and every executor of an execution has the same front end |
| **a front-end image of the dependency layer** (#274, #40): the front end's results for the dependency crates, laid out to be mapped read-only and validated by root, published by a trusted builder, keyed by `FrontEndKey` and the dependency sources | not built. #274's target: a warm restart under 1 s against 35.4 s on nushell (**reported**, n not stated); its kill criterion is in roadmap.md section 5 | **chosen** (unit D3): a new executor maps it instead of loading; executors on one machine share its pages; a pull request's executor starts warm without touching trusted state it could write |
| an executor loads only the closure of the home crates it serves | `tracedecay-framing`'s closure: a 3.0 GB worker against the CLI's 58.3 GB (TRACEDECAY.md, **read**); which share of the CLI's work homes in a small closure is not measured, and workspace-specialised instances home in the workspace (re-review B, M2) | **second stage** (unit D2): needs option (a)'s descriptor and D2's home inventory first |
| fork a warm trusted daemon | not measured | rejected (section 2) |
| ship MIR | no serialised MIR exists; codegen asks the database for layouts, vtables, type ids and symbols (no-rustc.md, **read**) | rejected |

### Where it pays, and what is measured

On tracedecay every cold or identity-changing build is the daemon's whole build (LOADMEM: nine
`opt-level=1` crates), and its codegen ran past 640,000 functions at about 25,000 a second on 18
cores before the guard stopped it, its end unmeasured (LOADMEM, one run, contended). The front end
before it took 120 to 210 s (TRACEDECAY.md, two runs) and is serial on the lead: distributing
codegen does not shorten it. A cold executor cannot pay on such a build. Distributed compile pays
where codegen is large and front ends are warm or shared:

- **a new blaze identity** (a blaze upgrade): every function is compiled again. The new binary's
  executors are new processes, so they load cold or from an image the new identity accepts, and
  this scenario's time includes those front ends (re-review B, M4);
- **a signature edit in a crate many depend on**: it renames every checkout crate downstream
  (cache.md), so their functions compile again until per-item keys (#344) land;
- **a lock-file change**: the dependency layer's instances compile again;
- **the first push of a pull request** whose executors start from the image;
- **a cold `Main` build of a new commit range** after the cache was emptied (a new blaze identity, section 1; the epoch bump of section 2's record
  is out of scope).

Gate GD measures these on tracedecay, not on a body edit (which compiles a few functions). If the
common cold first push must gain too, the front end has to be shared or distributed by crate; D3's
image is the first part of that, and D4 records whether the front end is the limiter.

### Memory per machine

In the first stage every executor holds the whole front end. From fleet.md (**reported**) and the
numbers above, each executor reserving its size plus 20%, rounded down to whole executors; the
arithmetic is **unverified** and is replaced by measured reservations (section 5) once P0-C and
P0-G have numbers. `B` is Bazel's reserve on Ubuntu-main; the `bazel-reserve` gate's default is 32
cores and 64 GB until Bazel's use is measured.

| Machine | RAM | Held back | Whole tracedecay front end today (58.3 GB, macOS, 1 run; with 20%, 70 GB) | At P0-C's bar (16 GB; with 20%, 19.2 GB) | Small closures (3 GB; with 20%, 3.6 GB), second stage only |
|---|---|---|---|---|---|
| Ubuntu-main, 128 cores | 256 GB | 16 GB system, B = 64 GB | ⌊176 / 70⌋ = 2 executors (the Linux size is unmeasured; macOS's footprint counts compressed pages Linux does not have) | ⌊176 / 19.2⌋ = 9 | many; CPU binds first: blaze stops scaling near 16 cores, so 8 executors of 16 cores is a starting partition |
| Mac Studio, M1 Ultra | 64 GB | 8 GB system | 0 | ⌊56 / 19.2⌋ = 2 | ⌊56 / 3.6⌋ = 15 by memory; its 20 cores bind first |
| 12-core desktop | 64 GB | 8 GB | 0 | 2 | 15 by memory; its 12 cores bind first |
| 12-core laptop | 16 GB | 4 GB | 0 | 0 | ⌊12 / 3.6⌋ = 3: helpers for low crates and test shards only, if the owner makes it a worker |

The row that decides the most is Ubuntu-main: with P0-C met, one trusted builder and several
pull-request executors fit beside Bazel's remaining work. With the image (D3), executors on one
machine share the dependency layer's pages, and each adds only what its own crates and edits
dirty (**unverified** until D3 measures it).

### Gate GD, and what changes if it fails

**Measured** (unit D4): tracedecay `tracedecay-cli`, the five scenarios above, on 1 executor (the
lead alone, running the same protocol with no helper), 2 executors on two machines, and 3 on
three; executors warm; at least 3 runs per arm, interleaved, one session; stage timestamps and the
work set's counters (items, batches, grants, leases revoked, duplicate compiles, bytes moved,
front-end load per executor, the lead's tail by reason).

**Correctness bar** (every run): the program's tests give the same verdicts as the one-executor
build; every entry key present in both has the same digest; the lead ran 0 compile queries for
accepted names an executor exported (GP's counted set, section 4); 0 compiler execs.

**Speed bar** (re-review B, M4): end to end. On the identity-change and signature-edit scenarios,
the job's time from `Queued` to `Done`, with its critical path named from the stage timestamps
(front ends, joins, batches, uploads, import, assembly, link, tests): every run of 2 machines
faster than every run of 1, and every run of 3 faster than every run of 2. The compile-phase ratios
of revision 2 (`Loaded` to `Compiled` of the lead, 1.4 times on 2 machines, 1.8 on 3) stay as
diagnostics, not the bar. On a body edit, distribution costs at most 5% end to end over 1
executor (the lead compiles small work alone: the coordinator grants nothing under a size set from
this measurement).

**If the speed bar fails, the limiter named by the stage timestamps decides what changes**, in
this order, and GD is measured again:

1. **The coordinator or the network** (grants, uploads, imports dominate): larger batches, packs
   uploaded while compiling, the lead importing as packs arrive.
2. **The executors' front ends** (they spend longer loading than compiling): executors kept warm
   across jobs by `FrontEndKey`; the image shared (D3) before executors across machines.
3. **Memory** (an executor cannot hold the front end): shrink it (P0.4's list), share its pages
   (D3). Smaller batches do not make a front end smaller (re-review B, M2); only D2's descriptor
   lets a helper hold less, and only for work homed in a smaller closure.
4. **Codegen itself is small** next to load and link for a scenario: that scenario's builds lead
   alone, by the batch-size rule above, and GD's speed bar applies to the scenarios where it is
   not. The work set stays on for every build; only small builds grant nothing.

### Warm remote jobs

A job (build, then tests) runs on one lead executor of the right trust, platform and
`FrontEndKey`, chosen by admission (section 5): a trusted builder for `master`, the pull
request's own executor otherwise. The lead takes the job's tree as an edit, runs the build with
the work set above, publishes, runs the tests (section 6), streams events, and stays at that tree.
This is revision 1's mode C, renamed: it moves a job, and is how distributed compile is reached,
not a replacement for it (review B M1).

**Results.** Whoever runs a program links it: a developer's `cargo blaze run` links on the
developer's machine from the store; a CI job's programs are linked by its lead. The splice and
live edits stay on the machine of the running program; they are never remote.

**Failure.** A lead that misses three heartbeats loses its job: the job is reassigned once with
a new attempt (the fencing table above keeps what was committed), and a second loss fails it with
the reason. A helper that misses them loses only its lease. `LocalPolicy`:
`Never` on hosted runners (tracedecay's own reason: "Fleet compilation must not fall back onto
the smaller hosted VM"), `Fallback` (build locally after the deadline or a remote failure),
`Race` (build locally from the start, per job, never per function). Under `Race` the first to
finish wins, but a first-finishing failure fails the job, as Bazel's dynamic execution does
(research 7.4), so local and remote differences surface. Locally, every policy runs under the
same no-rustc rule.

## 5. Admission and scheduling

- **Capacity per machine** is declared by its agent: cores and memory, less what is held back
  for the system and for Bazel's remaining work on that machine (the `B` of section 4; the owner
  sets it).
- **Reservations.** Each executor, each job and each test shard reserves memory and CPU before it
  starts, from measurement: an executor its `FrontEndKey`'s measured front end at ready, plus the
  largest compile peak recorded for it, plus 20%; a test shard the largest measured process tree
  of its tests plus their declared children; unknown sizes take the largest seen on that machine.
  Reservations are subtracted while running (NativeLink's `minimum` properties, research 7.12)
  and returned at the end. CPU is enforced on Linux by the executor's cgroup (`cpu.max`) and on
  macOS by blaze's thread count. Memory is enforced on the executor's whole process tree, not on
  one worker: on Linux by the cgroup's `memory.max`; on macOS by the agent, which sums the tree's
  footprint every second and stops the executor past its reservation with a named reason; and
  inside blaze by `RA_JIT_MEMORY_MAX`, which bounds what the daemon's workers hold together and is
  passed, unchanged, to every blaze child (own-script and own-macro compiles) through an immutable
  worker context rather than the environment `own_script.rs:253-280` clears (re-review B, M6).
- **Order (priority).** `master` builds before pull requests, then first in, first out, as
  NativeLink's priority orders its queue. A pull request's next push
  cancels its previous job's execution if nothing else subscribes to it.
- **Bounded queue.** At most `Q` executions waiting per platform (default 32); past it, `POST
  /v1/jobs` answers `busy` with a retry time and the requester follows its `LocalPolicy`.
- **Rebuilds** of a trusted builder (new identity, new `FrontEndKey`, growth past twice its ready
  size, 200 jobs, or daily: memory.md items 2, research #25) start beside the old one only if both
  reservations fit; otherwise the old one drains, stops, and the new one loads. No rebuild runs
  past the machine's capacity.
- **Stage timestamps** (`Stage`) on every job and every batch, so every wait has a name:
  waiting for admission is capacity, waiting after admission for an executor is matching, and so
  on. This is what tracedecay's measurement lacked (the note, line 67).
- **No hints in keys.** CPU counts, memory sizes, machine names and timeouts schedule; they never
  enter an entry key, a `CrateKey`, a `JobContent` or a `TestKey`. (In Bazel `cpu_count` is in
  every Rust action's key, research 7.7.)
- **Platforms.** A job runs on an executor whose `ExecPlatform` digest is its spec's. Linux
  x86-64 on Ubuntu-main and the Linux desktop; macOS arm64 on the Mac Studio. macOS jobs go
  only to macOS executors and Linux jobs never to the Mac Studio, as today. A pull request's macOS
  leg stays on its hosted macOS runner with no remote, as today; were the owner to join it later,
  its platform digest would differ from the Mac Studio's (the SDK skew of
  `docs/ci-remote-builds.md:10-12`), so it would share function entries (same triple and
  identity) but not programs or test verdicts.
- **Gate GB, the burst** (owner-operated): K concurrent pull-request jobs, K = 1, 3, 6, of the
  same three edit kinds, against NativeLink's fleet build of the same commits, in one session,
  3 runs per arm; push-to-verdict p50 and p95, and each wait named by stage. Bar: p95 no worse
  than NativeLink's at K = 3 and 6, with Bazel's reserve held. This is what decides whether the
  fleet's capacity is enough, not G3's single-job latency (review B M1).

## 6. Distributed tests

### What is cached: the contract (parity: Bazel's)

A result is reused as Bazel's `--cache_test_results=auto`, its default, reuses it
(research 7.10, **reported** here, read there): a test reruns only when its declared inputs changed. The key is what
the test action declares: the test program, its declared runfiles (here the `RuntimeBundle`), its
arguments, its declared environment and the platform. Time, randomness and thread scheduling are
not in it, as in Bazel (**unverified**, inferred from what an action holds).

1. **Declared inputs.** The `TestKey` is the digest of the program, the test's name, the
   `RunSpec`, the platform and the `RuntimeBundle`'s digest, with each declared file's digest,
   mode and length in the `InputManifest`. Nothing is recorded by running the test; a file a test
   reads that its bundle does not declare is not in the key, as with an undeclared read under
   Bazel. A test that needs a toolchain declares it as `rust_test`'s runfiles did.
2. **An opt-out marker.** A test the owner's manifest tags `external` or `no-cache` (Bazel's
   names) always runs. So does a test run with `--runs_per_test` above 1.
3. **Failures are never cached.** A failure is reported, and the next run runs the test: "a
   previous failure always reruns", as Bazel does, "as otherwise we can get stuck forever in the
   event of a flaky failure" (research/bazel-nativelink.md, citing `TestConfiguration.java:169-183`; **reported** here, read
   there).
4. **Flaky retries as Bazel's.** A test runs once, or up to three times when the manifest marks it
   `flaky`, as `--flaky_test_attempts=default` runs it (research/bazel-nativelink.md, citing
   `ExecutionOptions.java:240-252`; **reported** here, read there). One that passes on a later
   attempt is reported FLAKY and its successful result is cached: Bazel retains `testPassed=true`
   and `cachable=true` when changing PASSED to FLAKY (`StandaloneTestStrategy.java:228-239,678`),
   and auto caching accepts it (`TestRunnerAction.java:576-588`; **read** in the T1 review).
   What a FLAKY test does to the tracedecay job stays the owner's `flake-policy` gate below.

How many of tracedecay's tests the contract caches is measured (gate GT), not assumed. Time is not
virtualised and `HashMap`'s seed is not fixed: a test that reads either and passes is cached like any
other passing test, as under Bazel, and the owner's `no-cache` marker is the remedy for a test that
should not be.

### Local affected-test implementation (T1)

`RA_JIT_TESTS=affected` retains its existing per-test reach identities. In `blaze-tests.toml` at
its workspace root, `data` declares files and directories and `env` names variables; the names
in `RA_JIT_TEST_ENV` count too. Their contents, the effective configured environment,
`--test-threads` and `RUST_TEST_THREADS` are digested into each pass. Changing them reruns the
tests. `no_cache` and `external` name tests that always run, by libtest name, `package/name` or a
pattern with `*` at an end. An unreadable manifest or declared input prevents reuse.
Directory boundaries are part of the typed tree digest. Declared inherited environment
values are forwarded to Linux sandboxed tests, with configured command overrides preserved.

The manifest's `flaky` list gives a test three attempts, or `RA_JIT_FLAKY_TEST_ATTEMPTS` sets the
number for all tests; otherwise a local test gets one attempt. Only failed tests retry, and an
exhausted failure does not block another test's retry. A later pass is reported `Flaky` and
cached. The existing Linux read-recording sandbox and the existing conservative behavior when
no `data` is declared are unchanged by T1; no new effect classifier or audit mode is added.
The remote `TestKey` and runtime bundle below belong to T2 and T3.

### Out of scope (owner, 2026-10-26): the stricter contract

Earlier revisions held blaze's recorded key to a stricter contract than Bazel's declared one: an
always-run classifier over a test's reach (time, randomness, threads, sockets, unlisted spawns,
foreign calls outside a short list, unlisted environment), the sandbox's record of every path a
test read as its manifest, no caching at all on macOS without that record, an audit mode that ran
everything uncached daily and revoked any cached pass it contradicted, and random sampling of 5%
of cache hits. The owner ruled it out as more than Bazel gave tracedecay. **No unit builds it and no
acceptance line depends on it.** The old rules were *statically
closed* (the reach touches no time, randomness, thread, socket, unlisted spawn, foreign call or
environment read; unit T1 added them as reasons to always run), *dynamically recorded*, *not
marked* and *on macOS*, and an idea to seed `RandomState` per test; revision 3.1's text of them is
in this file's history before the scope ruling.

### Finding a pass on a machine that did not run it

A machine computes the `TestKey` before it runs the test, from what it has (contract rule 1), so no
manifest has to be matched against recordings:

1. The machine computes the `TestKey`: the test program's digest, the test's name, the `RunSpec`,
   the platform and the `RuntimeBundle`'s digest.
2. It asks `POST /v1/results/lookup` with the key in the namespace the server serves (at parity,
   one). The answer lists the passes kept for it with their manifests.
3. It checks the manifest's declared files against its own bundle: same digest, mode and length.
   Paths are logical (`$CHECKOUT`, `$OUT_DIR/<unit>`, `$BUNDLE`), resolved on each machine.
4. A manifest that holds in full is the pass's. The test is skipped and the report says so:
   "skipped: passed under the same inputs at abc1234". If none holds, or the test is marked
   `external` or `no-cache`, it runs.
5. A passing terminal result publishes its manifest and verdict together, including a success
   after retries, which retains the `Flaky` verdict. A terminal failure publishes nothing.
   Any job-level policy that rejects flaky results is separate from cache eligibility.

### Runtime bundles and shards

- A test program that runs on a helper gets its `RuntimeBundle` by digest: the program, every
  `CARGO_BIN_EXE_*` and other program it may run, the dynamic libraries outside the platform, the
  fixture files and `OUT_DIR` outputs it may read, its cwd and environment. A git checkout is not a
  bundle. The lead builds the bundle; helpers materialise it from blobs into their own directory.
  A test that needs a tool outside the bundle fails on a helper with the tool's name: it is
  declared or the test stays on the lead.
- Shards are cut from one test binary by each test's last recorded duration, longest first onto
  the least loaded executor of the platform; a test with no record counts as the median.

### Flakes and policy

- tracedecay's policy stays: a failure fails the job; `retries = 1` with `flaky-result = "fail"`
  is the nextest file's, and changing it is the owner's decision. Revision 1's "an unchanged flaky
  test does not fail" is withdrawn (review B M10). Attempts are Bazel's (contract rule 4); what a
  test that needed a retry does to the job is the `flake-policy` gate, default fail.
- **Audit mode and sampling are out of scope (owner, 2026-10-26)**: a daily uncached run of every
  test with revocation of caching, and 5% of cache hits run anyway. Bazel did neither.

### Tests that run rustc or cargo: an owner gate

Three of tracedecay's tests run rustc or cargo themselves and assert what they print. A worker
runs blaze, never rustc ([no-rustc.md](no-rustc.md)), and the owner's ruling does not police job
code: nothing at parity denies those execs on a blaze executor, and a test that needs a toolchain
declares it in its bundle as Bazel's `rust_test` runfiles did. Revision 2 kept them on Bazel as if
that were this design's choice; re-review A called it a regression of the Rust replacement's scope
(b-M8). Where they run is the owner's: the `rustc-tests` gate, open, with four options.

- **Keep them on Bazel** (the gate's default until the owner answers). S2's inventory marks each
  such test "stays on Bazel"; GS requires it executed and passing on Bazel and absent from blaze's
  side; it is not counted as missing.
- **Declared toolchain.** They run on blaze executors with the toolchain in their bundle, as under
  Bazel. The rustc that runs is the test's own program's behaviour, not blaze compiling (the
  superset rule), and it puts rustc runs on the fleet; whether the owner wants that is the gate's
  question. No unit is needed beyond T3's bundles.
- **The compatibility lane.** blaze gives these tests a `rustc` and a `cargo` on their `PATH`
  that are blaze: `cargo` is `cargo-blaze`, `rustc` is blaze's answerer extended to a
  diagnostics front end that checks a file and prints rustc-compatible diagnostics, built from
  blaze's own front end, placed in the tests' bundle by digest. Whether the tests' asserted output
  is matched is measured per test; a test whose output blaze cannot match stays on Bazel by name.
  This is a unit the owner can ask for (S4 below), not part of the default plan.
- **Change the tests**: the owner's repository, the owner's call.

## 7. Disk, bounded on every tier

Each tier has a default, a command and a harness check (the owner's requirement, 2026-10-26).

| Tier | Today | Bound | Default | Command | Check |
|---|---|---|---|---|---|
| Local store | LRU by pack within `RA_JIT_CACHE_MAX`, a tenth of free disk, 256 MiB to 32 GiB (`ra-jit-store/src/lib.rs:89-96`), run automatically | unchanged | as today | `blaze store collect` (exists as `StoreDir::collect`; C7 gives it a command) | `harness/storeguards.py`, plus C7's identity case |
| Stale identity, local | packs of an old blaze identity stay until LRU reaches them | packs no record or root of the current identity names are collected first | on every collect | `blaze store collect --stale` | C7: upgrade the binary, collect, old identity's packs gone, the new identity's build compiles what it must |
| Target directory | unmanaged; blaze's is 23.0 to 27.3 GB against cargo's 14.2 GB on tracedecay (TRACEDECAY.md), up to 3.9 times cargo's elsewhere (biome 42.5 GB against 11.0, benchmarks.md) | blaze's own files in it (its objects, programs, `.ra-jit-*` records) within `BLAZE_TARGET_MAX`, oldest program first, never the running program's | 2 times the size cargo's own build leaves, measured at the first build | `blaze clean --keep-latest` | C7: twenty builds of moving commits stay within the bound; P0.7 explains the extra 8.9 to 13.1 GB first |
| Cache server | none (revision 1: GC by roots) | section 3's GC: `--max`, low-water 80%, roots by generation, leases, completeness, hourly | `--max` 70% of the server's disk | `ra-jit serve collect --dry-run` | C7: 30 `master` commits and 10 pull requests with new generic instances each, within budget, the last 20 roots hit, obsolete instances gone |
| Executors | — | an executor's work directory is deleted when its job ends; a builder's worktree is one path, moved by edits, and its daemon unloads checkouts that are gone (memory.md item 3) | at job end | `blaze-agent prune` | E1: after 10 jobs, no work directory or process left |
| Stale identity, server | — | records of identities not served are dead after 7 days | | `ra-jit serve collect` | C7 |

## 8. Wire protocol

- **Encoding.** Deterministic CBOR as in "Data shapes", a validator that refuses any message not
  in that form (naming the first offence), JSON only as a rendering (`Accept: application/json`,
  `--json`). One crate, `crates/blaze-wire`, holds the schema for this API and the agent API
  ([roadmap.md](roadmap.md#8-the-agent-api)). minicbor is named by the roadmap and is not in
  `Cargo.lock` yet (**read**).
- **Transport.** HTTP/1.1 on the `hyper` and `rustls` already in the tree, `application/cbor`;
  inside the tailnet WireGuard encrypts, outside it TLS. An event stream is a chunked body of
  items, each a four-byte big-endian length and one CBOR item (the roadmap's socket framing).
- **Blobs** are not CBOR: `GET /v1/blob/<sha256>` with `Range`, `PUT /v1/blob/<sha256>`, checked
  before the server answers. Compressed objects as REAPI's ByteStream names them: by the digest of
  the uncompressed bytes, with the encoding in a header.
- **Versioning.** `POST /v1/hello` carries the versions a client speaks; the answer picks one and
  says the server's key, the epoch of each namespace, the identities served and the limits:
  largest request (16 MiB), largest event (1 MiB), largest blob, most items in a lookup (4,096),
  most items in a batch, longest string (4 KiB). A decoder refuses unknown keys within a version;
  a change that adds a field is a new version; the server keeps the version before for one blaze
  release.
  The current wire crate speaks schema 4 with native environment values and `/v4`
  typed digest/signature domains. Unchanged structural types may decode old bytes:
  version negotiation and digest/signature domains gate their schema identity.
  Changed types reject incompatible layouts/paths; a retained prior-version endpoint
  needs its separately pinned codec. HTTP `/v1` routes
  and `blaze/req/v1` authentication framing remain unchanged.
- **Errors** are one shape on every call: `{0: code, 1: message, 2: retry_after_ms or null}`, with
  HTTP status from the code's class. Codes: 1 malformed (400), 2 version (400), 3 unauthenticated
  (401), 4 not allowed (403), 5 not found (404), 6 conflict, a generation or attempt moved (409),
  7 too large (413), 8 busy (429), 9 refused by blaze, with the reason (422), 10 internal (500).
- **Credentials** (re-review A N2). `hello` carries none. `auth` carries the GitHub OIDC token in
  its body and nothing else; it is how a requester first gets a capability. A worker agent signs
  its requests with its enrolment credential, an Ed25519 key whose public half the operator
  registered: `Blaze-Agent: <agent name>`, `Blaze-Date: <unix ms>` and `Blaze-Signature:
  <base64url Ed25519 over "blaze/req/v1" ‖ 0x00 ‖ method ‖ 0x00 ‖ path ‖ 0x00 ‖ sha256(body) ‖
  0x00 ‖ the date>`; the server refuses a date more than 60 s off and a signature it has seen. The
  agent's own calls (`/v1/workers/hello`, `/v1/workers/next`) carry only that, and enrolment confers
  no other operation. Every other request carries `Blaze-Capability: <base64url of the canonical
  CBOR {0: Capability, 1: signature}>`; one an agent makes for an executor carries both.
- **Calls** (every request but `hello`, `auth` and the agent's own two carries a capability):

  | Call | Request | Answer |
  |---|---|---|
  | `POST /v1/hello` | `{0: versions, 1: StoreIdentity, 2: ExecPlatform, 3: role}` | `{0: version, 1: server key, 2: epochs, 3: identities, 4: limits}` |
  | `POST /v1/auth` | `{0: GitHub OIDC token}` | `{0: Capability, 1: its signature}` or error 3, 4 |
  | `POST /v1/cache/lookup` | `{0: namespaces in order, 1: set of CrateKey}` | `{0: per key, the newest CrateRecord or null}` |
  | `POST /v1/cache/missing` | `{0: set of Digest}` | `{0: the ones the server lacks}` |
  | `GET`, `PUT /v1/blob/<sha256>` | bytes | bytes |
  | `POST /v1/cache/publish` | `{0: CrateKey, 1: set of PackRef, 2: generation read, 3: SourceRef}` | `{0: the signed CrateRecord}` or error 6 |
  | `POST /v1/results/lookup` | `{0: namespaces in order, 1: set of TestBaseKey}` | `{0: per key, the passes with manifest digests}` |
  | `POST /v1/results/publish` | `{0: list of TestVerdict}` | `{0: count kept}` |
  | `POST /v1/roots` | `RootManifest` | `{}` |
  | `DELETE /v1/ns/pr/<n>` | | `{}` |
  | `POST /v1/jobs` | `{0: BuildSpec, 1: namespace, 2: deadline, ms since the epoch, 3: LocalPolicy, 4: test timeout ms or null}` | `{0: JobContent, 1: ExecutionId, 2: subscriber id}`; an equal `JobContent` running already is joined, not started again; fields 2 to 4 are the subscriber's, not the content's |
  | `GET /v1/jobs/<execution>/events?from=N` | | stream of `{0: sequence, 1: attempt, 2: event}`; `from` replays after a reconnect |
  | `DELETE /v1/jobs/<execution>/subscribers/<id>` | | `{}`; the execution is cancelled when its last subscriber leaves |
  | `POST /v1/workers/hello` | `{0: agent name, 1: capacity, 2: list of ExecutorState}` | `{0: accepted}` |
  | `POST /v1/workers/next` (long poll) | `{0: free reservations, 1: FrontEndKeys held}` | an assignment `{0: ExecutionId, 1: Attempt, 2: BuildSpec, 3: Capability, 4: role, lead or helper}`, or nothing |
  | `POST /v1/work/join` | `{0: ExecutionId, 1: Attempt, 2: RootSet, 3: set of WorkName rejected}` | `{0: the execution's RootSet, 1: whether the lead's has fixed it}`; error 6 (a different `RootSet` after the lead joined); a helper's provisional `RootSet` that differs from the lead's is fenced (section 4) |
  | `POST /v1/work/lease` | `{0: ExecutionId, 1: Attempt}` | a `Grant`, or nothing when the work set has none for this holder |
  | `POST /v1/work/result` | `BatchResult` | `{0: set of WorkName granted to the holder from its found names, 1: set of WorkName that became a conflict}` or error 6 (a dead lease generation) |
  | `POST /v1/workers/events` | `{0: ExecutionId, 1: Attempt, 2: list of events}` | `{0: continue or cancel}`; heartbeats ride on it |

- **Joining from a hosted runner.** The Tailscale action with an OIDC client whose only scope is
  auth keys for `tag:blaze-ci`, which may reach only the server's port; an ephemeral node, logged
  out at the job's end. Then a GitHub OIDC token for `POST /v1/auth`. Two independent checks, as
  tracedecay does for Tailscale and its workflow condition.
- **Rejected.** gRPC on the hot path; REAPI as the wire (section 3); JSON on the wire; the
  daemon's JSON-line socket protocol over TCP (per user and local by design, `socket.rs`).

### The shapes of the calls

Every request and answer above is a named struct, a CBOR map with every field present, its keys
the numbers below in ascending order, as "Data shapes" encodes them. The answers to a lookup are
in the order of the request's keys, so a client reads them without a map. Where this section and
"Data shapes" are silent, the simplest encoding is taken and said in one line after the code.

```rust
struct HelloRequest {               // 0..=3
    versions: Vec<u32>,             // 0, set: the versions the client speaks
    identity: StoreIdentity,        // 1
    platform: ExecPlatform,         // 2
    role: u8,                       // 3, as Capability's role: 0 requester, 1 executor,
                                    //    2 trusted builder, 3 worker agent
}

struct HelloAnswer {                // 0..=4
    version: u32,                   // 0, the one the server picked
    key: [u8; 32],                  // 1, the server's Ed25519 public key
    epochs: Vec<(Namespace, u64)>,  // 2, set: the current epoch of each namespace
    identities: Vec<StoreIdentity>, // 3, set: the store identities served
    limits: Limits,                 // 4
}

struct Limits {                     // 0..=5
    request: u32,                   // 0, bytes, 16 MiB
    event: u32,                     // 1, bytes, 1 MiB
    blob: u64,                      // 2, bytes, 4 GiB
    lookup_items: u32,              // 3, 4,096
    batch_items: u32,               // 4, 65,536
    string: u32,                    // 5, bytes, 4 KiB
}

struct AuthRequest { token: String }             // 0..=0, the GitHub OIDC token

/// The answer to `auth`, and the bytes of `Blaze-Capability`.
struct SignedCapability {            // 0..=1
    capability: Capability,          // 0
    signature: [u8; 64],             // 1, by the server
}

struct CacheLookupRequest {          // 0..=1
    namespaces: Vec<Namespace>,      // 0, in order of preference, not a set
    keys: Vec<CrateKey>,             // 1, set
}

struct CacheLookupAnswer { records: Vec<Option<CrateRecord>> } // 0..=0, per key

struct CacheMissingRequest { blobs: Vec<Digest> }  // 0..=0, set
struct CacheMissingAnswer { missing: Vec<Digest> } // 0..=0, set

struct CachePublishRequest {         // 0..=3
    crate_key: CrateKey,             // 0
    packs: Vec<PackRef>,             // 1, set
    generation: u64,                 // 2, the one the publisher read
    source: SourceRef,               // 3
}

struct CachePublishAnswer { record: CrateRecord } // 0..=0, signed

struct ResultsLookupRequest {        // 0..=1
    namespaces: Vec<Namespace>,      // 0, in order of preference, not a set
    keys: Vec<TestBaseKey>,          // 1, set
}

struct ResultsLookupAnswer { results: Vec<Vec<TestPass>> } // 0..=0, per key, in key order

struct TestPass {                    // 0..=2
    key: TestKey,                    // 0
    manifest: Digest,                // 1
    duration_ns: u64,                // 2
}

struct ResultsPublishRequest { verdicts: Vec<TestVerdict> } // 0..=0, order kept
struct ResultsPublishAnswer { kept: u64 }                   // 0..=0

/// The answer of every call that has nothing to say: `{}`.
struct Ack {}

struct JobRequest {                  // 0..=4
    spec: BuildSpec,                 // 0
    namespace: Namespace,            // 1
    deadline_ms: u64,                // 2, ms since the epoch
    local: LocalPolicy,              // 3
    test_timeout_ms: Option<u64>,    // 4
}

struct JobAnswer {                   // 0..=2
    content: JobContent,             // 0
    execution: ExecutionId,          // 1
    subscriber: u64,                 // 2
}

/// One item of an event stream, each behind its four-byte big-endian length.
struct EventItem {                   // 0..=2
    sequence: u64,                   // 0
    attempt: Attempt,                // 1
    event: JobEvent,                 // 2
}

struct WorkersHelloRequest {         // 0..=2
    agent: String,                   // 0
    capacity: Reservation,           // 1
    executors: Vec<ExecutorState>,   // 2, order kept
}

struct WorkersHelloAnswer { accepted: bool } // 0..=0

struct WorkersNextRequest {          // 0..=1
    free: Vec<Reservation>,          // 0, the reservations it can take work under
    front_ends: Vec<FrontEndKey>,    // 1, set
}

/// The long poll's "or nothing" is field 0 as `null`, not an absent map.
struct WorkersNextAnswer { assignment: Option<Assignment> } // 0..=0

struct Assignment {                  // 0..=4
    execution: ExecutionId,          // 0
    attempt: Attempt,                // 1
    spec: BuildSpec,                 // 2
    capability: Capability,          // 3
    role: u8,                        // 4, 0 lead, 1 helper
}

struct WorkJoinRequest {             // 0..=3
    execution: ExecutionId,          // 0
    attempt: Attempt,                // 1
    roots: RootSet,                  // 2
    rejected: Vec<WorkName>,         // 3, set
}

struct WorkJoinAnswer {              // 0..=1
    roots: RootSet,                  // 0
    fixed: bool,                     // 1, whether the lead's has fixed it
}

struct WorkLeaseRequest {            // 0..=1
    execution: ExecutionId,          // 0
    attempt: Attempt,                // 1
}

/// Nothing is field 0 as `null`, as for `WorkersNextAnswer`.
struct WorkLeaseAnswer { grant: Option<Grant> } // 0..=0

struct WorkResultAnswer {            // 0..=1
    granted: Vec<WorkName>,          // 0, set
    conflicts: Vec<WorkName>,        // 1, set
}

struct WorkersEventsRequest {        // 0..=2
    execution: ExecutionId,          // 0
    attempt: Attempt,                // 1
    events: Vec<JobEvent>,           // 2, order kept
}

struct WorkersEventsAnswer { verdict: u8 } // 0..=0, 0 continue, 1 cancel

struct WireError {                   // 0..=2
    code: u8,                        // 0, 1 malformed to 10 internal
    message: String,                 // 1
    retry_after_ms: Option<u64>,     // 2
}
```

The `Blaze-Agent` signature is over the bytes
`"blaze/req/v1" ‖ 0x00 ‖ method ‖ 0x00 ‖ path ‖ 0x00 ‖ sha256(body) ‖ 0x00 ‖ the date`, its header
the Ed25519 signature in base64url without padding. `Blaze-Capability` is `SignedCapability`'s
canonical CBOR, base64url without padding. The `ops` bits of a `Capability` are 0 lookup, 1 fetch,
2 publish, 3 events, 4 cancel, 5 roots, 6 delete a namespace, 7 work; a bit set is allowed.

The digest newtypes "Data shapes" defines over a map take their input as these structs:

```rust
struct CrateKeyInput {               // 0..=3
    identity: StoreIdentity,         // 0
    triple: String,                  // 1
    settings: Digest,                // 2, the entry settings of cache.md
    stable: Digest,                  // 3, sha256 of the crate's stable id
}

struct FrontEndUnit {                // 0..=9, a UnitSpec without its source id
    name: String,                    // 0
    version: String,                 // 1
    target: (u8, String),            // 2
    features: Vec<String>,           // 3, set
    cfg: Vec<InputText>,             // 4, set
    edition: u16,                    // 5
    deps: Vec<(u32, String)>,        // 6, set
    test: bool,                      // 7
    host: bool,                      // 8
    profile: ProfileSpec,            // 9
}

struct FrontEndKeyInput {            // 0..=5
    units: Vec<FrontEndUnit>,        // 0, in the canonical unit order
    lock: Digest,                    // 1
    cfg_rustflags: Vec<InputText>,   // 2, set: the rustflags that set cfg
    toolchain: StoreIdentity,        // 3
    triple: String,                  // 4
    workspace_manifest: SourcePath, // 5
}

struct ConfigKeyInput {              // 0..=6
    kind: u8,                        // 0, Command's discriminant
    asked: Vec<(String, String)>,    // 1, set: package name and target name
    profiles: Vec<ProfileSpec>,      // 2, each asked unit's, in the canonical unit order
    platform: Digest,                // 3
    rustflags: Vec<InputText>,       // 4, set
    toolchain: StoreIdentity,        // 5
    workspace_manifest: SourcePath, // 6
}

struct JobContentInput {             // 0..=1
    spec: Digest,                    // 0, BuildSpec's digest
    namespace: Namespace,            // 1
}

struct TestBaseKeyInput {            // 0..=5
    binary: Digest,                  // 0
    test: String,                    // 1
    run: RunSpec,                    // 2
    platform: Digest,                // 3
    sandbox: u32,                    // 4, policy version
    bundle: Digest,                  // 5
}
```

`TestKey`'s input is the two-element array `[TestBaseKey, InputManifest's digest]`, and
`WorkName`'s is the `WorkKey` itself, so neither needs a struct. `StoreIdentity` is ra-jit-store's
`Identity::new` unchanged and `EntryKey` its own `sha256(identity ‖ key)`, so neither has an input
struct: the two values are the store's, and their digests below are only for golden vectors and for
a blob that names them. Every enum's payload is a map of
the fields its variant names, with these numbers:

```rust
SourceRef::Git { repo_id: u64, commit: [u8; 20], tree: [u8; 20] } // 0..=2
SourceRef::Root { root: Digest }                                  // 0
Namespace::Pr { n: u64 }                                          // 0
Command::Test(RunSpec), Command::Run(RunSpec)                     // payload: RunSpec's map
WorkKey::Item { unit: Digest, stable: Digest, env: Digest }        // 0..=2
WorkKey::Edge { parent: WorkName, ordinal: u32 }                   // 0..=1
ItemOutcome::Exported { entry: EntryKey }                         // 0
ItemOutcome::FromStore { entry: EntryKey }                        // 0
ItemOutcome::Failed { why: String }                               // 0
ItemOutcome::Local { why: String }                                // 0
ItemOutcome::Lost { why: String }                                 // 0
Observation::File { path: String, digest: Digest, mode: u32, len: u64 } // 0..=3
Observation::Dir { path: String, names: Digest, mode: u32 }       // 0..=2
Observation::Absent { path: String }                              // 0
Observation::Link { path: String, target: String }                // 0..=1
JobEvent::Stage { stage: Stage, at_ms: u64 }                      // 0..=1
JobEvent::Assigned { executor: String, attempt: Attempt }         // 0..=1
JobEvent::Published { records: Vec<(CrateKey, u64)> }             // 0
JobEvent::Program { name: String, blob: Digest }                  // 0..=1
JobEvent::Verdict(TestVerdict)                                    // payload: TestVerdict's map
JobEvent::Refused { function: String, why: String }               // 0..=1
JobEvent::Output { stream: u8, bytes: Vec<u8> }                   // 0..=1
JobEvent::Done { outcome: Outcome, counters: Vec<(String, u64)> } // 0..=1
```

`LocalPolicy`, `Stage`, `Outcome` and `Namespace`'s other variants are units: payload `null`; a
`Pr` is `[1, {0: n}]`, the payload map the list above gives it. A newtype with no field numbers is
its inner value, unwrapped. Each payload map is a struct of its own, and `crates/blaze-wire` names
them `GitRef`, `RootRef`, `PrNamespace`, `ItemKey`, `EdgeKey`, `EntryRef`, `Why`, `FileObservation`,
`DirObservation`, `AbsentObservation`, `LinkObservation`, `StageEvent`, `AssignedEvent`,
`PublishedEvent`, `ProgramEvent`, `RefusedEvent`, `OutputEvent` and `DoneEvent`.
`JobEvent::Verdict`'s and `Command::Test`'s payloads are the payload type's own map, not a map that
holds it, so a decoder reads the enum as an array of two.

What this leaves to the implementation, in one line each: `HelloRequest.role` and
`WorkersEventsAnswer.verdict` are `u8` (an enum there would be a two-element array for a byte);
`Limits.blob` is 4 GiB and `Limits.batch_items` 65,536, the two the section does not number;
lookup answers follow their request's key order; `CrateKeyInput.stable` and
`TestBaseKeyInput.binary` are digests, not names; a `Vec` not marked set keeps its order;
`TestPass` is the "pass with manifest digest" the table names in prose; `[u8; N]` and a digest are
byte strings, and a `Vec<u8>` is an array of integers as any `Vec` is.

## 9. Upstream's objections

- **"Per-function `Execute` and per-function ActionCache lookups lose"** (#226, ci-runner.md).
  Agreed with their numbers (165,752 lookups, 18.6 s serial). Nothing here sends or looks up one
  function: the units are a batch of thousands, a crate's record, a test shard and a job.
- **"Width beyond one box buys about 5%"** (ix, a 192-thread box, ci-runner.md, **reported**).
  Conceded for one job on one big box, and Ubuntu-main is a big box. What this design does
  instead of arguing it:
  - It does not claim queue relief. The measured queue (51.8%, one release build) has no named
    limiter; GB measures concurrent pull requests against NativeLink.
  - One blaze daemon stops scaling near 16 cores (4.5 s on 16, 3.9 s on 96 for the standard
    library). Several executors on Ubuntu-main splitting one build's batches is the first width GD
    measures, before machines.
  - "The `tracedecay` crate's 200 s pole does not exist in the same shape under blaze" is
    **unverified**: rustc parallelises codegen within a crate too, and the measured run was a
    `k8-opt` release build this design leaves on Bazel (research 5). P0-H and GD measure blaze's
    own time for that crate.
  - Distributed compile is required by the owner; GD decides how it is done, not whether.

## 10. What stays on Bazel

| Kept on Bazel | Why |
|---|---|
| Windows | blaze builds nothing on Windows yet ([usage.md](usage.md), **read**) |
| JS, Python | Rust only, by scope |
| release and dist (thin LTO) | blaze has no LTO; its release programs ran at 1.02 to 1.35 times cargo's release run time on the bench apps ([research/codegen-units.md](research/codegen-units.md), **read**, run time, not build time) |
| Clippy | a rustc driver, which blaze never runs |
| Linux ARM | no ARM worker in the fleet |
| tests that run rustc or cargo | section 6: the owner's `rustc-tests` gate; its default keeps them here, the compatibility lane (S4) would move them |
| Rust built as an input of a JS or Python test | stays rules_rust until the owner settles it |

**How they coexist.** A `rust-blaze` job runs beside Bazel in shadow mode, then blocking, then
`linux-test`'s fleet shard and the macOS Rust tests leave Bazel. Each step is the owner's, in
tracedecay's repository, after a gate (section 11). The comparison is not "equal verdicts on the
tests both ran": it is against an inventory (unit S2) of every Rust test target, feature
variant, custom harness, doctest and generated fixture Bazel runs today, approved by the owner,
with each test's timeout, retries and resource group as Bazel runs it. It fails on any test
missing or extra on either side, on a test neither executed nor validly skipped (a skip counts
only when its cached pass's manifest held, section 6), on a test the inventory keeps on Bazel that
Bazel did not execute, and on a different verdict. Bazel keeps NativeLink for what stays on it, with a reserve on Ubuntu-main
(section 5). The git dependency fetch of 487.687 s is Bazel's crate-universe rule (research 5);
blaze fetches git dependencies and submodules itself (`blaze-registry/src/git.rs`), once per
executor, which G2 measures.

## 11. Gates

Rules: at least 3 runs per arm, interleaved, in one session, on the named machine; median,
spread and the limiter named from `RA_JIT_STATS` or stage timestamps; a ratio only against a
same-session baseline. "Where" names who can run it: **Mac** (this machine, heavy lock, guard),
**Linux** (a Linux host of at least 64 GB, the owner's `linux-host` gate), **Docker** (8 GB VM on
this Mac, small fixtures), **fleet** (owner-operated, in tracedecay's CI; the unit lands the
tooling and the report form, the owner runs it and the report unit records it).

| Gate | Before | Measurement | Bar | Where |
|---|---|---|---|---|
| **P0-A to P0-H** | every unit after Phase 0 that names them; never the Phase 0 unit that remediates the predicate | section 0 | section 0 | Mac; P0-G Linux |
| **GP, the two-process prototype** | D1 (no network layer before it; both stages) | section 4, "The first unit": GP-fixtures, then GP-full after its preflights | section 4 | GP-fixtures: Docker on the Mac until P0.6, then the Mac; GP-full: the Mac, native, after P0-H |
| **G1, cross-machine hits** | C5's switch to per-crate records | `harness/dist/crossmachine.py`: machine B (other `HOME`, `CARGO_HOME`, checkout path, target and store) builds A's commit from A's published records | ripgrep and nushell: 0 functions compiled on B; tracedecay: at least 99% of functions from the store on B, every miss listed with its cause; programs print on B what cargo's builds print there, panic paths included | Mac (two machines on one host); Linux |
| **G2, the cache in CI** | S1's blocking step | hosted runner, tracedecay test programs from nothing: no remote, warm `Main`, and Bazel's fleet build of the same commit | warm `Main` no slower than Bazel's fleet build; functions compiled only in the affected closure of the commit's changes (the edited crates and the checkout crates downstream, cache.md), until #344 | fleet |
| **G3, one job** | E3's switch | a warm lead against Bazel's fleet job, same commits: body edit in a leaf crate, body edit in the `tracedecay` crate, signature edit in a crate many depend on | push to verdicts faster than Bazel's fleet job for each, 5 runs each | fleet |
| **GB, the burst** | S3's blocking step | section 5 | section 5 | fleet |
| **GD, distributed compile** | D4 closes; S3's blocking step | section 4 | section 4; a failure changes how, by section 4's list | Mac and Docker (two executors, small fixtures); Linux; fleet (tracedecay) |
| **GT, tests** | T2's switch to skipping | tracedecay's suite under the parity contract (section 6): share of tests cacheable; then 20 runs of an unchanged commit; then changes to a declared input (a bundle file's content or mode, an argument, a declared environment variable) and a `no-cache`-marked test | every run of an unchanged commit skips each cacheable test whose declared inputs are unchanged; every change to a declared input runs the affected test; a marked test always runs; a failure is never skipped; the share cacheable recorded | Mac; Linux; fleet for the suite |
| **G6, requests per build** | C5 | server counters | one lookup request per build; 0 per-function requests; server CPU per job recorded | Mac |
| **GS, shadow** | the owner's removal of Bazel's Rust paths | two weeks of shadow mode on `master` and pull requests, against S2's inventory | every inventoried test executed or validly skipped (its cached pass's manifest held) on blaze's side with Bazel's verdict, every test the inventory keeps on Bazel executed there, or each difference filed and fixed; 0 missing or extra (re-review A, b-M10) | fleet |

## Phase plan

Each unit is one pull request against `dist/main` in the fork, from its own worktree on
`dist/<unit>` cut from `origin/dist/main`, opened with `gh pr create --repo ScriptedAlchemy/blaze
--base dist/main`; never `land.py`, never upstream. Each lands alone and is checked by its verify
command, which runs every acceptance assertion and exits non-zero when one fails. Harnesses run
with `RUST_BACKTRACE=0`. Every blaze run on the Mac runs under the guard and takes the heavy lock
for its duration.

`BIN` in every command is built once per unit and session:

```sh
export CARGO_TARGET_DIR=/Users/bytedance/dev/blaze-targets/<unit>
cargo +1.99.0 build -p ra-jit --release
export BIN="$CARGO_TARGET_DIR/release/ra-jit"
```

A unit with a Docker stage also builds `BIN_LINUX_AARCH64` inside Docker, from its own pinned
image and its own target directory, under the same wrapper and never beside another heavy build
(D0 names the form); a Mach-O `BIN` does not run in a container.

Fixture repositories and commits are pinned in `harness/dist/fixtures.toml` and fetched by
`python3 harness/dist/fetch.py` (unit P0.1), into `~/work/<name>` at the pinned commit. Paths
under `harness/dist/`, the crates `blaze-wire`, `blaze-serve` and `blaze-dist`,
`crates/ra-jit/src/worker.rs`, `crates/ra-jit/src/remote/serve.rs` and
`docs/reports/DIST-SHADOW.md` are new, made by the unit that first names them; every other path
exists at `f9b5da46`; so do `crates/blaze-spawn`, `crates/hir-ty-codegen/export.rs`,
and `harness/spawnsites.py` once their units make them.
"Depends" names units and the Phase 0 predicates a unit needs met. A unit after Phase 0 whose
acceptance runs tracedecay needs P0-H on that platform. A Phase 0 unit never depends on the
predicate it remediates or on P0-H, which P0.8 establishes from them; its own tracedecay runs are
its acceptance, not a prerequisite (re-review A N3).

### Phase 0: blaze builds tracedecay

**P0.1 The tracedecay harness, guarded and audited.** Goal: one harness for every Phase 0
predicate on both platforms. Extends `harness/tracedecay.py` with: the program's guard as a default
it cannot turn off; `norustc.py`'s stand-in rustc, rustdoc and cargo on `PATH` and as `RUSTC`,
`RUSTDOC`, `CARGO`, counting every run with its argv; a `--predicate P0-x` mode that runs that
predicate's scenarios and exits non-zero unless its bar holds; `fixtures.toml` and `fetch.py`.
Files: `harness/tracedecay.py`, `harness/dist/fixtures.toml`, `harness/dist/fetch.py`,
`harness/norustc.py` (the stand-ins as a module). Depends: none. Acceptance: on hello-world, the
audit counts the rustc runs today's `blaze build` makes (more than 0) and `--predicate P0-A` fails
naming them; the watchdog's self-test (a synthetic allocator, not blaze) is killed at a 512 MB test
limit and recorded. From the reviews:
- [ra-N4] "Restore the standing guard or obtain an explicit owner exception, and verify the full
  process tree." Checked by: the watchdog applies the guard's three rules (summed `top` MEM past
  24 GB; swap used grown by at least 1 GB since the start while free swap is under 3 GB; swap used
  grown by 8 GB), each with a self-test, and sums the run's whole process tree: a self-test child
  started outside the binary's directory that allocates past the test limit gets the tree killed.

Verify: `python3 -I harness/tracedecay.py --td ~/work/hello --predicate P0-A --expect-fail &&
python3 -I harness/tracedecay.py --self-test`. Where: Mac.

**P0.2 The no-rustc rule.** Goal: P0-A's mechanism, as section 0 specifies it: `crates/blaze-spawn`
for every spawn site of `crates/` and the vendored launchers; blaze's answerer for compiler
probes, with `answers.rs`'s fallback removed; the audit and provenance records of blaze's own run.
**Out of scope (owner, 2026-10-26)** and not in this unit: the process-tree boundary of section 0
(the mount namespace, `noexec` mounts, the exec store, Landlock, the seccomp filter and its ABI
check, the Seatbelt profile, `sandbox.rs`'s exec authorisation and the macOS sandbox file), the
negative compiler fixtures that tried it and the racing-exec harness. Files: `crates/blaze-spawn`
(new),
`crates/ra-jit/src/{main,answers,link,splice,shipped,stub,socket,doctest,test_harness}.rs`,
`crates/ra-jit/src/cargo/{mod,state,wrapper,script,daemon,run,whole,own_script,own_macro,client}.rs`,
`crates/ra-jit/src/bin/{ra-jit-rustc,cargo-blaze}.rs`, `crates/blaze-build-script/src/{main,run}.rs`,
`crates/hir-ty-codegen/{asm,fncache}.rs`, `crates/ra-jit-store/src/lib.rs`,
`crates/blaze-registry/src/git.rs`, one hook each in `vendor/rust-analyzer/crates/stdx/src/process.rs`
and `proc-macro-api/src/process.rs`, `harness/spawnsites.py`, `harness/dist/spawn-allow.toml`,
`harness/dist/fixtures/norustc/`. Depends: P0.1. Acceptance: with deny, a project blaze cannot
build without cargo fails before any spawn, naming the reason; `spawnsites.py` passes; the audit
counts 0 compiler execs started by blaze on hello-world on Linux (Docker), where the whole build
works. From the reviews (the parts that needed the dropped boundary are in appendix A as accepted
at parity):
- [rb-B1] parity remainder. "Audit actual descendant execution with absolute-path compiler,
  renamed executable, shell/linker wrapper, script/proc-macro/test spawning cargo, and cargo
  aliases/plugins." What parity keeps is blaze's own spawns; a job's descendants are not policed.
  Checked by: every fixture of section 0's rule (project-model's `cargo check --compile-time-deps`,
  `cargo metadata` without `--no-deps`, a cargo alias, a `cargo-foo` plugin, `RUSTC_WRAPPER` and
  `RUSTC` naming a compiler, a cold dispatch, an unanswerable probe) ends in a named refusal from
  blaze's spawn API or answerer, 0 compiler execs started by blaze, and the build failing only where
  it needed the refused spawn; the positive fixtures (`pnpm` from a build script, `autocfg`,
  `version_check` and `rustversion` probes, a `cc` build) pass.
- [a-B1] parity remainder. "gating Command::new inside ra-jit does not mediate other crates or
  execs made by job descendants; PATH stand-ins do not audit absolute compiler execs." Checked by:
  fixtures that make blaze's own code spawn a compiler by absolute path, from `blaze-build-script`,
  `asm.rs`, `blaze-registry/src/git.rs` and the vendored launchers, are each refused by name; a
  spawn outside `blaze-spawn` that `spawn-allow.toml` does not list fails `spawnsites.py`. A
  compiler a job's own descendant starts is not checked.
- [rb-M5] "Require successful cold CLI build, first edit, test-binary construction/execution, macro
  expansion and doctest on both platforms; expected refusal is only a separate negative fixture.
  Explicitly assign the missing proc-macro/test/check paths before P0-H can close." Checked by: the
  provenance audit (cold isolated target, store and daemon home; every Rust artifact linked or
  loaded recorded with origin and digest) passes on hello-world and ripgrep here, and on
  `tracedecay-cli` in P0.8; `own_macro.rs`'s two fallbacks and the loaded symbol files are gone in
  worker mode, and `whole.rs` makes Test and Check whole as it does Build and Run, each with a
  test.

Verify: `cargo test -p blaze-spawn -p ra-jit spawn && python3 -I harness/spawnsites.py && python3 -I
harness/tracedecay.py --td ~/work/hello --predicate P0-A --linux-docker && python3 -I
harness/tracedecay.py --predicate P0-A --fixtures norustc`. Where: Mac, Docker.

**P0.3 A loading worker is bounded.** Goal: P0-B. The worker checks its own footprint during
load and its build, and past `RA_JIT_MEMORY_MAX` stops with "the worker holds N MB, over the N MB
of RA_JIT_MEMORY_MAX, while loading"; `compile_ahead` takes its work in batches and, past the
budget, ends the batch, hands what it compiled on and releases its memos (LOADMEM fixes 2 and 3);
`over()` (`cargo/daemon.rs:96-104`) no longer exempts a worker alone over the ceiling; in worker
mode the stop fails the job. The program's unit u7-bound is implementing this; its batch is the
batch D0 distributes, so its hand-on step takes a sink (the object locally, the exporter in D0).
Files: `crates/ra-jit/src/cargo/daemon.rs`, `crates/ra-jit/src/main.rs` (load path), `memory.rs`,
`crates/hir-ty-codegen/mod.rs` (`compile_ahead`'s batches), `crates/ra-jit/src/cargo/own_script.rs`
and `own_macro.rs` (the budget passed to children). Depends: P0.1. Acceptance: `tracedecay-cli` at
the guard's `RA_JIT_MEMORY_MAX=16G`: the worker stays within 1.05 times the bound, or stops within
2 s of passing it, the build says why, the watchdog never fires; with the bound above the worker's
size (a small project) nothing changes. No run on the Mac sets a bound below 16G (the guard);
smaller bounds run in Docker. From the reviews:
- [rb-M6] "Extend P0-B to simultaneous busy workers, concurrent script/macro compiles and a child
  that consumes memory, measuring the summed tree against the executor's reservation." Checked by:
  two workers busy at once, four own-macro compiles at once and a build script that allocates are
  each measured as one summed tree; the workers together stay within 1.05 times
  `RA_JIT_MEMORY_MAX`, which keeps its meaning of what workers hold together; own-script and
  own-macro children receive the bound from the worker context although `own_script.rs:253-280`
  clears the environment.
- [ra-N4] "The revision's new 2G/512M predicate runs ... also need a clearly documented exception
  to the fixed Mac environment, rather than silently changing it." Checked by: the Mac runs use
  16G on `tracedecay-cli`; the 2G and 512M cases run only with `--linux-docker`.

Verify: `python3 -I harness/tracedecay.py --predicate P0-B && python3 -I harness/tracedecay.py
--td ~/work/nushell --predicate P0-B --memory-max 2G --linux-docker`. Where: Mac, Docker.

**P0.4 A tracedecay worker under 16 GB.** Goal: P0-C. LOADMEM's ranked fixes after P0.3's batch,
stopping when P0-C holds: MIR not kept once an instance has code (`transform.rs:192,218`), eviction
between batches of crates during resolution (`resolve_crates`), the type GC
(`hir::collect_ty_garbage`) and the allocator's purge between batches, `cached::Entries` and
`StableNames` keyed by hashes, the symbol table as a view of its file; then memory.md's reclaim at
idle (the program's unit u6), recycle on growth, one worktree path per worker. Each step lands as
its own commit with its measured effect. Files: `crates/ra-jit/src/{main,memory,symbols}.rs`,
`cargo/daemon.rs`, `crates/hir-ty-codegen/{mod,cached,stable}.rs`, and in
`vendor/rust-analyzer` the MIR queries of `hir-ty`. Depends: P0.3, LOADMEM.md (landed). Acceptance:
P0-C's bar on macOS, 3 runs; each step's effect in TRACEDECAY.md with spread. Verify: `python3 -I
harness/tracedecay.py --predicate P0-C --runs 3`.
Where: Mac. If 16 GB is not reached, the unit lands what it reached and the owner decides between
the Linux host and a higher bar (open questions).

**P0.5 Build scripts rerun as cargo reruns them.** Goal: P0-D. Parks PR #5 (unit u8,
`dist/u8-buildscript`), which failed four verifications on its interactions with the state cargo
keeps in `target/debug`, and does its job in the whole build. The build-script runner of units
p05w and p05r stays in scope: blaze compiles and runs scripts with its own records in
`target/ra-jit/build-scripts`, without `cargo check` when the graph has no proc macros. A graph
with proc macros still uses the load's check; that remaining path is no-rustc step J.

The runner decides when to compile or run a script by cargo 1.99's rules
(`blaze_build_script::fresh`): a file named by `rerun-if-changed` that is newer than the run or
not there, a variable whose value changed (unset, empty and set are three), the package's files
when the script named nothing, the script's own modules and the packages it is built with, and
a run owed for having said other than the run before. It judges a script once for each build
(a build is several requests to a worker, which cargo's every-build scripts must not follow).
The scripts run with the variables of the process that asks; a variable the daemon was started
with that the client lacks is unset for them. A run uses the `OUT_DIR` it left and is not
replayed from the store. An impure script runs once per build, its latest output used
(recording its reads where blaze's existing sandbox does, #147), and is never shared across
machines. Files: `crates/ra-jit/src/scripts.rs`, `crates/ra-jit/src/inputs.rs`,
`crates/blaze-build-script`, `crates/ra-jit/src/cargo/own_script.rs`,
`crates/ra-jit/src/cargo/daemon.rs` (the build a request is for), `harness/scriptedits.py`.
Depends: P0.1. Acceptance: `scriptedits.py`'s 42 probes, each blaze's whole build against cargo
1.99.0 on the same project and edits, equal at every step in the build's status, how many times
each script ran, and the program's exit status, standard output and standard error; each fix
has a unit test; the tracedecay fixture is P0-D's. From the reviews:
- [rb-N2] "Specify the number of measured edit and revert builds per run, one consistent
  denominator (for example 30/30), and whether the cold/warm preparation builds are excluded."
  Checked by: `--predicate P0-D --runs 5` counts 30 builds (5 runs × 3 edit kinds × an edit and its
  revert), excludes the one cold and one warm build before them, and passes only at 30 of 30 the
  daemon's, 0 `building with cargo`.

Verify: `python3 -I harness/scriptedits.py && python3 -I harness/tracedecay.py --predicate P0-D
--runs 5`. Where: Linux (Docker on the Mac until P0.6); the P0-D run on the Mac once P0.6 lands.

**P0.6 The whole build on macOS arm64 (#255).** Goal: P0-E. The standard library's writable statics
without a rustc symbol (`CLEANUP`, `GLOBAL_PANIC_COUNT`, `OUTPUT_CAPTURE_USED`, `INSTANCE`) are
blaze's own on macOS, as the whole-program path makes them on Linux, in doctests and build scripts
too. Files: `crates/hir-ty-codegen/mod.rs` (statics), `crates/ra-jit/src/cargo/whole.rs`,
`crates/ra-jit/src/cargo/own_script.rs`. Depends: P0.2. Acceptance: hello-world, ripgrep and
`harness/doctestonce.py` on macOS with `BLAZE_WHOLE_BUILD=1`, programs' output equal to cargo's; 0
`without a rustc symbol`; the 107 build scripts of tracedecay compiled by blaze. Verify:
`python3 -I harness/wholeprogram.py && python3 -I harness/doctestonce.py && python3 -I harness/tracedecay.py --predicate P0-E`.
Where: Mac.

**P0.7 A cold build's peak and target directory.** Goal: P0-F and the target directory's extra
8.9 to 13.1 GB explained. Script compiles go to the daemon that holds the standard library, or at
most `cores / 4` run at once (#203, memory.md item 4); the target directory's contents are listed
by owner and the extra is named. Files: `crates/ra-jit/src/cargo/own_script.rs`, `sandboxed.rs`,
`cargo/daemon.rs`, `harness/tracedecay.py` (`--disk`). Depends: P0.6. Acceptance: P0-F's bar on
tracedecay; a table of the target directory by producer in TRACEDECAY.md. From the reviews:
- [a-m2] "P0.7 verify never checks its disk table" Checked by: `--predicate P0-F --disk` lists the
  target directory by producer and fails unless the producers' sizes add up to the directory's size
  within 1% and the extra over cargo's is attributed to named producers.

Verify: `python3 -I harness/coldprof.py --memory ~/work/ripgrep && python3 -I harness/tracedecay.py
--predicate P0-F --disk`. Where: Mac.

**P0.8 G0 again, on both platforms.** Goal: P0-G and P0-H. TRACEDECAY.md's scenarios with
blaze's whole build against cargo, n ≥ 3, limiters named; on Linux x86-64 at tracedecay scale
every P0 predicate. Files: `docs/reports/TRACEDECAY.md`, `harness/tracedecay.py` (a `--linux`
mode over SSH). Depends: P0.2 to P0.7; the owner's `linux-host` gate for Linux. Acceptance: every
P0 predicate met and recorded on macOS; on Linux, met and recorded, or the unit lands macOS and
marks Linux open. Verify: `python3 -I harness/tracedecay.py --predicate all --runs 3` (and
`--linux HOST`). Where: Mac; Linux.

### Phase 1: the wire, the specification of a build, and the cache

**C1 The wire schema.** Goal: "Data shapes" and section 8 as code. `crates/blaze-wire`: every
type, encode and decode, the section 4.2 validator, digests and signatures with their domain
separation, errors, limits, JSON rendering. Files: `crates/blaze-wire`, `Cargo.toml`, `Cargo.lock`.
Depends: none. Acceptance: every type round-trips byte-identically; golden vectors for one value
of every type, its digest and its signature, checked in; the validator accepts all the encoder
writes on random values and rejects non-shortest heads, indefinite lengths, unsorted keys,
duplicate keys, floats, tags, unknown keys and oversize items, naming each; the ordinary decoder
holds a message to the same size and depth limits as the validator; a lookup's collection is held
to 4,096 items. From the reviews:
- [a-M4] "enum payload maps and many endpoint response/limits types remain prose; capability
  transport/ops bits and JobContent bytes are unspecified, so C1 still must invent wire choices."
  Checked by: before its code, C1 adds to section 8 a named struct for every endpoint's request and
  answer and for `Hello`'s limits, and the payload map of every enum variant, with field numbers;
  golden vectors cover each of them, the capability headers of section 8, the `ops` bits and
  `JobContent`'s bytes as "Data shapes" now gives them.

Verify: `cargo test -p blaze-wire`. Where: Mac.

**C2 `BuildSpec` from cargo's resolution.** Goal: review B M4. A requester computes its
`BuildSpec`, `ExecPlatform` and `FrontEndKey` from the same resolution blaze's own build uses
(`blaze-resolve`, `blaze-manifest`, `cargo/cli.rs`), and refuses, before scheduling, any flag,
profile setting or environment it cannot honour, naming it. Files: `crates/ra-jit/src/cargo/cli.rs`,
`crates/blaze-resolve`, `crates/blaze-wire`, `harness/dist/buildspec.py`. Depends: C1.
Acceptance: differential fixtures: `--no-default-features`, `--all-features`, `-p` and
`--exclude`, two `RUSTFLAGS --cfg` values, `--ignored`, `--include-ignored`, `--no-run`,
`--no-fail-fast`, a `harness = false` test, `[env]` in config, a cwd below the root: each gives
a different spec where it changes the build and the same where it does not, and running the spec
locally does what the command does (same programs, same tests run); `lto = "thin"` is refused by
name; whether the deployment target changes a function's code is recorded. From the reviews:
- [b-M4] "no field carries blaze backend/MIR optimisation or per-package/build-script profile
  overrides; refusing already supported settings is not equivalent to local cargo blaze semantics."
  Checked by: differential fixtures with `[profile.dev.package.X] opt-level = 1` (tracedecay's nine
  crates), `[profile.dev.build-override]`, and blaze's backend and MIR-level settings: each is
  carried per unit (`UnitSpec.profile`, `ProfileSpec.backend`, `ProfileSpec.mir_opt`), running the
  spec locally does what `cargo blaze` does, and none of them is refused.
- [b-M5] "ExecPlatform omits C/C++ tool/input digests and native closure" Checked by:
  `ExecPlatform` gains the digests of the C and C++ compilers, archiver and linker a build script
  may run (as the `cc` crate resolves them) and of the SDK or sysroot; a fixture with a C dependency
  under two compilers gives two platform digests and two `JobContent`s.

Verify: `cargo test -p blaze-wire && python3 -I harness/dist/buildspec.py`. Where: Mac.

The graph tests compare all 317 recorded Cargo unit graphs. The graph keeps proc-macro
doctest libraries under their host/build profile, and a normal check and test check of the
same library or binary remain distinct.
The host debuginfo-sharing probe uses Cargo's explicit target kind, including for proc macros,
so a build-only unit cannot match itself and incorrectly retain the runtime debug level.
A custom test harness receives `cfg(test)` without acquiring the libtest harness. CLI checks
and Rust unit tests do not establish local execution acceptance or the macOS gate.

The CLI now saves the actual source inputs, including dirty and untracked files, selected
path/registry/Git dependency trees, configuration reads and relative symlink targets. A canonical
locked-source index belongs to that snapshot: changing which captured package directory a locked
source names changes the source identity. The original invocation is a separate CBOR replay recipe,
so equivalent command spellings retain the same `BuildSpec` and `JobContent`. The selected manifest,
config-file operands, native values and source-relative working directory survive relocation.
Replay authenticates the snapshot, resolves from its captured package trees offline, compares the
complete spec, and verifies the execution platform before creating compiler descriptions.
An empty external Cargo home does not affect source identity. A Cargo home beside or above the
workspace (its configuration, registry and Git trees) is kept in the snapshot under
`.blaze-cargo-home/`, so the layout root is made of the workspace and its path dependencies only:
the same project with its home in another place, or below other directories, has the same spec.
The configuration files resolution read are part of the replay record (version 2), in precedence
order, and every reading of the configuration during replay, the executor's included, is made from
exactly those through `Config::from_image`, with no discovery: a Cargo home that was an ancestor's
`.cargo` to the requester keeps its place, though the snapshot mounts it, and no file above the
directory a worker materializes the snapshot in is read. Path dependencies outside the workspace still keep their relative topology below the common
source root, so a workspace and a sibling path dependency under different parents differ. (Mac and
Linux unit tests; the layout independence of path dependencies is not normalized.)

`ra-jit cargo spec --out FILE build|run|test ...` writes canonical BuildSpec CBOR to `FILE`
and the immutable blobs plus replay recipe to `FILE.inputs/`. Copy both to replay elsewhere with
`ra-jit cargo spec --run FILE`. Successful artifacts and a `result.json` are retained under
`FILE.outputs/`. The original checkout is not needed. Each compiler description carries authoritative
cfgs, environment, dependency renames, target kinds and profiles; the loader cannot add analyzer cfgs
or substitute ambient compiler settings. Replay starts the owned compiler directly with compiler
query fallback, Cargo producer loading, external proc-macro servers and shared publication disabled.

The connected executor admits host Rust libraries as source dependencies of binaries and examples,
and has explicit paths for unit/integration test executables. `test --no-run` builds the test
executables without running implicit doctests. It refuses standalone library artifacts,
check/doctest/benchmark work, build scripts, proc macros, unsupported output types and incompatible
per-unit debug settings before starting compilation. Expanded application native link inputs are
refused until their complete closure can be bound. The existing `ExecPlatform.native_root` is still
a proxy for the language runtime's native inputs, not a complete SDK/library closure. A version-1
deterministic CBOR trace (`[version, files, [[name, value-or-null], ...]]`, with message-sized text
budgets) preserves application input paths and environment reads, including filenames that Cargo's
line-oriented dep-info cannot represent. Replay checks that trace against the snapshot before an
executable is advertised or run. This is admission after compilation, not a filesystem sandbox.

A program that `run` or `test` starts gets the environment Cargo gives one, not an empty one: the
worker's `PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, `TMPDIR`, `TZ`, `TERM`, `LANG` and `LC_*`; the
spec's configuration and script values; Cargo's package variables (`CARGO_MANIFEST_DIR`,
`CARGO_MANIFEST_PATH`, `CARGO_PKG_*`), whose paths are in the replayed tree; and `CARGO`, naming the
toolchain's cargo. The worker's other variables do not reach it, and the dynamic-library search path
Cargo extends is not set (programs are linked statically).

`harness/dist/saved_spec.py` exercises the actual binary against a Cargo reference with product
compiler tools blocked, including removal of the original tree and hostile worker configuration.
Its result, the broader `buildspec.py` differential, native closure coverage, other platforms and
the original complete C2 acceptance remain separate gates. C2 stays draft until those gates pass.

**C3 The server, today's layout.** Goal: `ra-jit serve --cache DIR --listen ADDR` with `hello`,
blobs with `Range`, `missing` from an existence set, verification on write, and today's
`manifests/`; a client place `Place::Serve` (`blaze+http://host:port`). Push, pull, lazy and the
keeper work through it unchanged. At parity (the scope section) it serves one namespace to every
tailnet peer with no credential. Files: `crates/blaze-serve`, `crates/ra-jit/src/remote.rs`,
`crates/ra-jit/src/remote/serve.rs`, `harness/remote.py`. Depends: C1. Acceptance: `remote.py`'s
round trip, damage, signing and kill checks pass against the server as a third leg; every call of
section 8 that carries a capability or an agent signature in the schema is served without one
(no `Blaze-Capability` header, no signature), answers in the one namespace `Main` at epoch 0, and
leaves a record's signature unset, with a test for each; the server listens on the address it is
given and `--listen` on a non-tailnet address is the operator's choice, not a check. Verify:
`python3 -I harness/remote.py --server`. Where: Mac.

**C4 Content-only keys across machines.** Goal: section 1's table, diagnosed and fixed.
`harness/dist/crossmachine.py` makes two machines on one host (own `HOME`, `CARGO_HOME`, checkout
path, target and store), builds on A, publishes to a directory remote, builds on B, and lists every
function compiled, script run and macro expanded on B by cause. Then the fixes: crate ids from
absolute paths, registry paths in stored code rebound symbolically if the fixture shows them, script
run keys and expansion keys by content and recorded reads. A fixture crate panics with a location,
uses `file!()`, has debug info, an `OUT_DIR` file, a path dependency outside the workspace, and a
vendored one. Files: `harness/dist/crossmachine.py`, `harness/dist/fixtures/paths`,
`crates/ra-jit/src/main.rs`, `crates/ra-jit/src/cargo/script.rs`, `crates/ra-jit-expand`,
`crates/hir-ty-codegen` (rebinding, if needed). Depends: P0.5; P0-E for tracedecay's row.
Acceptance: G1 on ripgrep, nushell and the fixture; B's programs print what cargo's print on B;
`harness/keys.py` and `harness/expansions.py` pass. Verify:
`python3 -I harness/dist/crossmachine.py --all && python3 -I harness/keys.py && python3 -I harness/expansions.py`.
Where: Mac; Linux for recorded reads.

**C5 Per-crate records.** Goal: section 3's lookup. A publish cuts packs by home crate and adds
`CrateRecord` generations with epoch and source; a daemon computes its `CrateKey`s at load, looks
them up in one batch, fetches only what it lacks; completeness on lookup; a determinism check (two
builds of one commit on A and B give equal entry digests). Files: `crates/ra-jit/src/remote.rs`,
`remote/keeper.rs`, `crates/ra-jit-store/src/lib.rs`, `crates/blaze-serve`. Depends: C3, C4, P0-A.
Acceptance: crossmachine through the server: G1 holds, one lookup request per build (G6), and with
two workspaces on the server B fetches only its own crates' packs; a record naming a deleted pack is
answered without it. From the reviews:
- [b-M5] "a platform field entering only CrateKey cannot separate unchanged local EntryKeys, and
  different-host portability is not in C5 acceptance." Checked by: a platform field C2 finds to
  change code enters the entry key's settings (`cached.rs`'s key), not only `CrateKey`, with a
  fixture where it does; crossmachine with machine B in a Docker container of another libc
  version: function entries are shared where no such field differs, programs and test verdicts are
  not (their `ExecPlatform` differs).
- [c3-B2.1] "Include canonical UnitSpec identity and semantic environment fingerprint where
  equivalence is not proved; apply the same distinction to store keys." Checked by: two requests
  that section 4's `WorkKey` tells apart (an instance asked under two param environments; a
  function present in two units) get two entry keys, or the exporter marks the pair `Local`
  rather than let one overwrite the other; on a fixture where the stable name alone would have
  merged them, a cross-machine build gives the right program and no entry of one request is read
  for the other.

Verify: `python3 -I harness/dist/crossmachine.py --all --remote server && python3 -I
harness/dist/crossmachine.py --remote server --b-docker`. Where: Mac, Docker.

**C6 Trust: the tailnet's access model.** Goal: section 2's access model, which is today's
(`docs/ci-remote-builds.md`), and nothing more. Reduced by the scope ruling: **out of scope
(owner, 2026-10-26)** are OIDC verification against a JWKS, the GitHub API lookups, capabilities,
roles, `Main` and `Pr(n)`, epochs, the forced rebuild, signed records and the fake issuer
(`harness/dist/github.py`, `auth.py`, `signed.rs`, `policy.rs`, `github.rs`). What remains: the
server and the agent listen on the tailnet address only (an option, default the tailnet address
when one exists and loopback otherwise); the deployment notes (`harness/dist/ci/README.md`'s access
section) state the Tailscale grants of section 2 and the owner step that adds them (write access
is limited to CI and fleet nodes by ACL, as it is today); the CI client of S1 does nothing for a
fork run. Files: `crates/blaze-serve/src/main.rs` (the listen address),
`harness/dist/ci/README.md`. Depends: C3. Acceptance: the server started with no `--listen` binds
the tailnet address, or loopback when the host has none, and says which, and a test checks both
cases; the README lists the grant for the CI tag and the grant for the fleet tag by port, and
states that Bazel's own grants need no change. From the reviews: [ra-N2] is accepted at parity
(appendix A): there is no credential to transport.

Verify: `cargo test -p blaze-serve listen`. Where: Mac.

**C7 GC and disk on every tier.** Goal: sections 3 and 7. Roots by `ConfigKey` with used-entry
lists, compaction with remapping, leases and pins, budget with low-water mark, hourly collection,
identity expiry; the local store's `--stale` collect
and command; the target directory's bound and `blaze clean --keep-latest`. Files:
`crates/blaze-serve/src/collect.rs`, `crates/ra-jit-store/src/lib.rs`, `crates/ra-jit/src/main.rs`
(commands), `harness/dist/gc.py`, `harness/dist/disk.py`. Depends: C5. Acceptance: gc.py's synthetic
publisher (it writes roots and generations through the API, no CI needed): 30 `master` commits each
adding generic instances to the same dependency crate, 10 pull requests opened and closed: within
budget, every one of the last 20 roots builds with 0 functions compiled, obsolete instances' packs
gone, a pinned generation survives a collection that overlaps a running lookup; disk.py: the target
directory bound and the stale identity case of section 7. From the reviews:
- [b-M9] "no used-entry closure tells compaction which entries are dead or how roots move to
  replacement generations." Checked by: after gc.py's 30 commits and a collection, the dependency
  crate's packs hold only entries some live root's `used` list names (counted against the union of
  the lists); every live root builds with 0 functions compiled, served through `remap`; a root that
  names a replaced generation is answered with the compacted one.
- [ra-N1] "Project an explicit source-independent configuration key for root retention ... and an
  explicit atomic root-remapping/indirection rule." Checked by: 30 `master` commits of one
  `ConfigKey` keep exactly its last 20 roots, and two `ConfigKey`s keep 20 each; a collector killed
  between writing g' and appending the remap leaves g live and g' unnamed (the remap is the commit
  point), and the next collection finishes the job.

Verify: `python3 -I harness/dist/gc.py && python3 -I harness/dist/disk.py`. Where: Mac.

### Phase 2: executors and jobs

**E1 Agents and executors, Linux.** Goal: workers as NativeLink's are, on Linux. Reduced by the
scope ruling: **out of scope (owner, 2026-10-26)** are section 2's isolation rules 1 to 6 (a fresh OS
user per executor, the whole-job sandbox, the trusted store and image mounted read-only, an
overlay, an unreadable credential, the destroyed pull-request executor) and the adversarial
test. What remains: `blaze-agent` registers with the server, long-polls for work, starts an
executor for a job in a work directory under the agent's work root (its own worktree, target
directory and temporary directory), runs it as one process group, kills the group at the job's end,
on cancellation and on the deadline, uploads its outputs and deletes the work directory. It keeps
builders warm on `master`. Lifecycle as a sans-IO state machine in `crates/blaze-dist`. Files:
`crates/blaze-dist`, `crates/ra-jit/src/worker.rs`, `crates/blaze-serve` (executor registry).
Depends: C3, P0-A, P0-B. Acceptance: the lifecycle's transitions as sans-IO tests (register,
assign, start, finish, cancel, deadline, agent restart); a job runs in
`<work-root>/<execution>/<attempt>` and nothing of the job is written outside it and the store; a
cancelled job, a job past its deadline and a finished one each leave no process of the group and
no work directory; after an agent restart `blaze-agent prune` removes the work directories
of jobs the agent no longer knows; after 10 jobs nothing is left. Verify: `cargo test -p blaze-dist
&& python3 -I harness/dist/executors.py --linux-docker`. Where: Docker; Linux.

**E2 Executors on macOS.** Goal: the same on macOS: the agent on the Mac Studio runs jobs in a work
directory as E1 does. **Out of scope (owner, 2026-10-26):** a user per executor and the Seatbelt
profile, and with them a pull request's macOS jobs on the Mac Studio as an isolation question; the
macOS pull-request leg stays on its hosted runner as today (section 5). Files:
`crates/blaze-dist`, `crates/ra-jit/src/worker.rs`. Depends: E1, P0-E. Acceptance: E1's lifecycle
test on macOS; a macOS job is given only to a macOS executor and a Linux job is never given to
one (checked by `ExecPlatform` matching). Verify: `python3 -I harness/dist/executors.py`. Where:
Mac.

**E3 Jobs.** Goal: warm remote jobs end to end, review A M6. `RA_JIT_DIST=URL` makes `cargo blaze
build|test` send its `BuildSpec`; content id, execution id, attempt, subscribers; events with
replay; cancellation when the last subscriber leaves; reassignment once with fencing; the
no-rustc rule; `LocalPolicy`, with a first-finishing failure failing a race. Files:
`crates/blaze-dist`, `crates/blaze-serve/src/jobs.rs`, `crates/ra-jit/src/cargo/client.rs`,
`crates/ra-jit/src/worker.rs`. Depends: E1, C2. Acceptance (`harness/dist/jobs.py`, two agents on
one host, small fixtures): results equal a local `cargo blaze test`; two identical requests with
different deadlines and policies share one execution, and cancelling one subscriber leaves the
other's running; a killed executor's job finishes on the other with attempt 2 and the dead
attempt's late events are refused; a job needing cargo fails naming why; `fallback`, `race` and
`never` behave as section 4 says. From the reviews:
- [a-M6] "independent subscriber deadlines, shared execution lifetime and timeout/fallback
  behavior for differing policies are still undefined." Checked by: the execution's deadline is the
  latest of its live subscribers'; a subscriber whose own deadline passes is detached with
  `TimedOut` and follows its own `LocalPolicy` (`Fallback` builds locally, `Never` fails), while the
  execution runs on for the others; the execution ends when its last subscriber leaves or the
  latest deadline passes; jobs.py covers two subscribers with deadlines 1 s apart and policies
  `never` and `fallback`.
- [a-M9] "no unit owns the real RootManifest publication client, so production GC still assumes an
  unassigned producer." Checked by: a finished job's lead writes its `RootManifest` (with `used`
  lists and `ConfigKey`) through the agent's `POST /v1/roots` before `Done`; a failed job writes
  none; C7's gc.py, pointed at jobs.py's server, finds the manifests the jobs wrote.

Verify: `python3 -I harness/dist/jobs.py`. Where: Mac, Docker.

**E4 Admission.** Goal: section 5. Capacities, measured reservations, cgroups on Linux, thread
counts on macOS, `Main` first, the bounded queue, rebuild by drain when overlap does not fit,
stage timestamps on every job and batch; the burst harness for GB. Files: `crates/blaze-dist`
(admission), `crates/blaze-serve/src/jobs.rs`, `harness/dist/burst.py`. Depends: E3. Acceptance:
on one host with a capacity set to two fixture executors, a third job queues with stage
`Queued` and a named wait; a `Main` job overtakes queued pull requests; the queue answers `busy`
past `Q`; a rebuild that does not fit drains first; no reservation ever exceeds capacity in a
1,000-job random schedule (a property test of the state machine). From the reviews:
- [rb-M6] "On Linux include the whole tree in a cgroup; on macOS use a supervisor/aggregate
  footprint guard as the production enforcement, not merely the development harness watchdog."
  Checked by: an executor whose test spawns a child that allocates past the executor's reservation
  is stopped with a named reason, by `memory.max` on Linux (Docker) and by the agent's tree
  footprint guard on macOS, while a second executor beside it keeps running; the stop fails that
  job, not the machine's other jobs.

Verify: `cargo test -p blaze-dist admission && python3 -I harness/dist/burst.py --local`. Where:
Mac, Docker.

### Phase 3: distributed compile

**D0 Two processes on one Mac (gate GP).** Goal: section 4's mechanism without a network: the
work key and pre-index, claimed discovery with retained findings, the exporter and its
`ExportContext`, the lead's store-hydrated assembly and the compile-query count, proven by a lead
and one helper splitting one build. `compile_ahead`'s batch (P0.3) gets a work
source and a sink behind two traits in `hir-ty-codegen`: locally the queue and the object, in D0
the coordinator and the exporter. The work set is the sans-IO state machine of section 4 in
`crates/blaze-dist` (join, grant, commit, done, the fencing table, conflicts, returned work),
driven in the lead's process over a Unix socket with section 8's `work` calls as CBOR. Its
transitions are tested sans-IO, with no sockets, in this unit; D1 adds only the HTTP layer and
its fault injection. Files: `crates/hir-ty-codegen/export.rs`
(new; the `ExportContext`), `later.rs` (`entry`, `keep` and `tracked` through the exporter),
`mod.rs` (`compile_ahead`'s source and sink, `hydrate` and `roots_from_store` for a distributed
lead, the compile-query counter from before load), `cached.rs` (`accepted_closure`, the sink's
durable finish), `stable.rs` (the work key, the `tuple` digest, the pre-index and the root list in
canonical order), `crates/blaze-dist` (new),
`crates/ra-jit/src/worker.rs` (new; the local driver), `crates/ra-jit/src/main.rs`,
`crates/ra-jit-store/src/lib.rs` (import of a pack), `harness/dist/proto.py` (`--platform`,
`--stage`, `--preflight`, `--capacity`), `harness/dist/fixtures/dist/`. Depends: P0.3 (u7's
batch), P0.2 (blaze's spawn rule on both processes), C1 and C2 (the wire types and the `BuildSpec` the
work calls carry); P0.6 for the Mac arm of GP-fixtures (Docker until then); for GP-full, P0-H on
macOS (so P0-E and P0-B with it) and the capacity preflight (c3-D0 prerequisites). Acceptance:
GP-fixtures: GP's bar (section 4) on `demo/mid` and `demo/real`, 3 recorded runs each; GP-full:
the same bar on `-p tracedecay --lib` under the guard, 3 recorded runs, when its prerequisites are
met, and `pending` with them named until then; every run records both executors' shares, grants,
duplicate compiles, the lead's tail by reason, the raw, accepted and export-invalidated sets and
each process's peak footprint. From the reviews:
- [rb-B3] "Make D1 test that lead compile-query invocations are zero for successfully imported
  work on both cold and warm signature edits, not just that the program is correct (which serial
  recompilation can also satisfy)." Checked by: on a cold build and on a signature edit of
  `demo/mid`'s workspace crate made after a warm build, the lead's counters, taken from before the
  front end loads, show 0 assembly-phase compile queries for accepted names and 0 queries for
  accepted names a helper exported, and every compile query it does run carries a reason.
- [a-M7] "normal assembly still disables checkout store hydration on warm patch builds and D1
  assigns no patch/base integration." Checked by: after the distributed signature edit, a body
  edit is a local patch of the hydrated build (its `tracked` kept), and its program equals a
  one-process build's.
- [rb-B2] "Add independent-DB round-trip/compile tests (different interning order, long nested
  types, closures, async/coroutines, const generics, caller bounds and repeated crate units),
  before D1 is declared implementable." Checked, for the first stage, by: two processes that load
  the same fixture with the crates resolved in different orders (so their interners differ) post
  the same `RootSet`, and name every root and instance with long nested types, closures, async
  functions, const generics, caller bounds and a unit present twice (host and target) alike. The
  descriptor's own round trip is D2's.
- [rb-M1] "Add tests with a cached root but evicted callee, cached generic but missing descendant,
  cyclic cache entries, missing debug type set and GC during upload/import. Measure lead fallback
  work and forbid false complete-coverage claims." Checked by: each of those five cases is a
  fixture; in each, the missing entry becomes work (not `FromStore`), Done is not reported until it
  is terminal, and the lead's tail names it.
- [rb-M7] "Add a private unused function containing an existing unsupported fixture beside a valid
  main, then call it in the next version: cold, warm and distributed builds must agree on which
  version succeeds and on the named error." Checked by: that fixture, run one-process cold, warm and
  distributed: the first version succeeds in all three and the second fails in all three with the
  same named error, and nothing goes to rustc.
- [b-M7] "there is no terminal lead-only outcome for an uncacheable fragment" Checked by: a fixture
  function the exporter cannot export (data holding the checkout's path, before C4) is `Local`, the
  lead compiles it and its callers, and the program is right.
- [c3-B2.1] "A name is not yet a total, environment-safe work identity." Checked by: (1) a fixture
  asked under two param environments (caller bounds, an opaque type) and one present in two units
  (host and target) get four distinct `WorkName`s and four entries, none overwriting another, and
  the entry key tells them apart or the colliding pair is `Local`; (2) a request with no stable
  name (a const argument `stable.rs:597-607` cannot name) is an `unnamed` entry of its parent, a
  terminal `Edge`, the parent is `Local`, and the lead rediscovers the request by compiling the
  parent, with the program right; (3) two finders reporting one name with different `tuple`s make
  a `Conflict`: nothing is granted or exported for it, its exported callers are not accepted, the
  lead compiles it and the conflict is counted; (4) a root list with a tied pair (same
  `WorkKey`, different `tuple`) swapped on one executor posts a different `RootSet`; (5) the
  **verification arm** compiles every colliding environment and unit case independently on both
  executors under different interning orders and compares code, requests, debug and tracking
  bytes; on any disagreement GP fails or the work routes `Local`, name equality alone not
  sufficing.
- [c3-B2.2] "Finder retention must be an explicit lifetime and lease contract." Checked by: a run
  with at least three real batch evictions and type GCs between a finding and its grant, over
  closures, coroutines, caller bounds and types past 1,024 bytes, in which every grant resolves;
  a holder whose finding was dropped (fault-injected) answers `Lost { lost-finding }`, stops being
  a finder, and the name goes to another finder or becomes terminal `Local`; a batch stopped at
  the budget returns its unprocessed ranges and names with its items, and no next generation is
  issued before they are back in the pool; on lead loss, stale attempt results are refused,
  outstanding roots and names are reassigned, dead attempts' findings are invalid, and a name with
  no finder left reaches the new lead's tail and is compiled by required-reachability, never by
  decoding a digest; a root-only result from a dead generation installs no names.
- [c3-B2.3] "Root agreement is an admission check, not full front-end agreement or a liveness
  proof." Checked by: (1) a helper joining first, then a lead whose `RootSet` differs, fences the
  helper and the execution restarts serially on the lead's inputs, with the determinism bug
  reported and the lead never refused; (2) two executors with equal sorted roots and an impure
  macro that changes a body (and a build script whose output changes a body) without changing the
  roots: they differ in `inputs`, or share the lead's published output, and the program equals a
  one-process build's; (3) a heartbeating helper that commits nothing is revoked by the
  progress timeout before any median exists; a root lost three times goes to the lead's tail; an
  execution with no compatible lead ends `Failed`, and one at its deadline `Cancelled`, each
  named and neither reporting `Done`; (4) the sans-IO tests also cover multiple finders, holder
  loss, partial batches, and a replay before and after a generation change.
- [c3-B2 pre-index] "The lead/helper must pre-index all eligible non-generic entries and on-demand
  functions into digest-to-local-root mappings ... An unknown digest must be a conflict/local
  outcome, never silently missing work." Checked by: an on-demand non-generic function reached
  only through a call (not a root) is granted by digest to either executor and compiled; a
  grant naming a digest in neither the holder's index nor its findings is `Lost { unknown-name }`,
  counted as a conflict, and the function ends compiled by the lead; two units sharing one
  spelling resolve to two index entries.
- [c3-B3.1] "ExportContext must resolve every request, not receive an object's already-resolved id
  maps." Checked by: a test per `FuncTarget` and `DataRequest` variant, each classified
  identically by the object-backed and the batch-backed caller: native, owned and shipped
  symbols, TLS, foreign and own statics, `TypeId` in a call *and* in a blob, a vtable and drop
  glue, `asm`, line-only and full debug, and a tracked checkout entry; a request outside the
  classifier's table is `Local`, never `Import`; a counter shows the classifier compiled nothing
  and mutated no object; the entry an object-free helper writes equals the one the object-backed
  `later.rs` writes for the same request; the policy digest of two executors and the lead is equal.
- [c3-B3.2] "'0 exported names compiled by the lead' conflicts with the Local closure rule."
  Checked by: a fixture whose exportable parent calls a generic the exporter marks `Local`: the
  parent is raw and export-invalidated with that reason, the lead compiles it (counted with the
  reason), and the program is right; a `Root` edge to a `Local` item invalidates its caller; the
  harness prints raw, accepted and export-invalidated sets apart and asserts 0 for the counted
  set of section 4 (accepted names), with a nonempty accepted set and both executors' accepted
  shares nonempty. Labelling all useful work `Local` passes nothing.
- [c3-B3.3] "Seal/import is a compiler-writer barrier and a cache generation boundary." Checked by:
  a lead that looked an entry up before import and found it absent finds it after (a fresh
  `Entries`); a batch whose last entry and last type set are written asynchronously is complete at
  the sink's finish and sealed with every key it lists, or the finish fails; after a signature
  edit rebuilt warm, an immediate body edit is a local patch of the fresh store-backed base with
  its tracked marks, and its program equals a one-process build's.
- [c3-D0.1] "Specify two platform builds, an actual Docker preflight and honest stage readiness."
  Checked by: `proto.py --platform macos|linux-docker --stage fixtures|tracedecay|all` builds and
  names `BIN_MAC` and `BIN_LINUX_AARCH64` as section 4 says; `--preflight` runs hello-world, a
  whole build with the standard library, and a two-process export and import on the platform, and
  on a Docker run without the pinned image's standard library or linker inputs it exits non-zero
  naming them; the stage statuses print as `closed`, `pending (needs ...)` or `failed`, and `--stage all` never
  reports `closed` for `tracedecay` while a prerequisite is unmet; three runs of each required
  arm are recorded, and D1's verify refuses to start without a report holding both stages
  `closed`.
- [c3-D0.2] "P0-C's one-worker 16 GB target does not guarantee the two-process 24 GB gate."
  Checked by: `proto.py --capacity` runs the aggregate preflight of section 4 for
  `-p tracedecay --lib`, with the harness, coordinator and both trees under one watchdog and one
  heavy-lock lifetime and every standing `RA_JIT_*` setting on, and writes the simultaneous
  aggregate footprint and swap series (1 s), 3 runs; a run over the guard is `failed: capacity`
  and leaves GP-full `pending`, never `closed` or skipped; the guard's values appear unchanged in
  the report.
- [c3-D0.3] "Count from before load, isolate the baseline, and make the oracle non-vacuous."
  Checked by: the trace starts before the front end loads and an injected ungranted compile query
  (a `lower_until` or root pre-pass compile) fails the run; the three phase counts are reported
  apart; the baseline is built cold and private, archived, and compared against, and a run seeded
  from the other arm's store is refused by the harness; the verification arm is separate from the
  sharing arm; the final program's origin table has a nonempty helper share with roots and
  discovered generic instances; the cases of section 4's bar (exportable parent of a `Local`
  child, multiple batch evictions before grants, duplicate finders, dead-lease recovery, a
  same-`RootSet` different-body conflict, zero-read and no-rustc provenance) are each a fixture.
- [c3-D0.4] "The gate falsifies this mechanism, not distributed compilation as a deliverable."
  Checked by: five injected faults each make `proto.py` exit non-zero with a named cause: a
  flipped byte in an exported entry; a dropped helper pack (a serial fallback); an ungranted
  compile query; a swapped tied root pair; an empty helper share. A failed GP prints that it
  falsifies the mechanism on that fixture and that D1 is blocked, and names the next step
  (work key, exporter, recovery strategy or D2's descriptor).

Verify: `cargo test -p blaze-dist work && cargo test -p hir-ty-codegen export && python3 -I
harness/dist/proto.py --platform linux-docker --preflight && python3 -I harness/dist/proto.py
--platform linux-docker --stage fixtures` (and `--platform macos --stage fixtures` once P0.6 has
landed); GP-full is closed by `python3 -I harness/dist/proto.py --platform macos --stage tracedecay
--capacity`, which prints `pending` with the unmet prerequisites until they are met. Where: Mac,
Docker.

**D1 The work set over the network.** Goal: section 4's protocol between machines: the work calls
of section 8 over HTTP, leases with heartbeats and progress timeouts, the fencing table, pins of
uploaded packs, several helpers and several leads. Files: `crates/blaze-dist`,
`crates/blaze-serve/src/work.rs`, `crates/ra-jit/src/worker.rs`, `harness/dist/shards.py`. Depends: D0
(both stages of GP closed, c3-D0.1), E3, C5, P0-A, P0-B. The sans-IO transitions of the work set are
D0's; D1 adds the HTTP transport and its fault injection. Acceptance (`shards.py`, a lead and two helpers on one host, each
with the whole front end, then two in Docker): GP's bar holds with two helpers; fixtures with
mutually recursive functions across crates, generics, trait objects, drop glue and statics; two
leads at once (neither waits on the other). From the reviews:
- [rb-M3] "Test loss after compile, after upload, after result commit but before acknowledgment,
  stale success during replacement, partial cache-hit batch, coordinator log recovery, and a
  heartbeating-but-stuck helper. Require no lost discoveries and eventual terminal execution"
  Checked by: a fault injector kills or stalls a helper at each of those points; in each run every
  root and found name ends terminal, the coordinator restarted from its log reaches the same
  state, the stuck helper's lease is revoked by the progress timeout, and the program equals the
  one-process build's.
- [ra-N5] "Specify one policy with assignment identity, not just an attempt and timestamp, and
  test it. Neither a lease timeout nor a missing cache entry should become an unexplained rejection
  of a program the one-executor build accepts." Checked by: a result carrying a dead `Lease.generation`
  is refused whole and its items end terminal through the replacement; no run's outcome differs from
  the one-executor build's because of a lease or a missing entry.
- [ra-N6] "Define an earlier dependency-closure seeding phase, or change that acceptance and cost
  model." Checked by: a warm helper joins and compiles roots before a cold lead finishes loading
  (stage timestamps), and the result equals the one-process build's.
- [b-M7] "tests omit tiny-pack and cancellation-during-tests cases." Checked by: a batch of one
  function (a pack of one entry) is exported, imported and used; a cancellation while the lead runs
  tests leaves no executor process and no pin.

Verify: `cargo test -p blaze-dist work && python3 -I harness/dist/shards.py && python3 -I
harness/dist/shards.py --faults`. Where: Mac, Docker.

**D2 Smaller helpers (second stage, conditional).** Goal: helpers that hold less than the whole
front end, only if it pays. First, the home inventory; then, only if it shows useful work that
fits smaller helpers, option (a) of section 4: the lossless, versioned instance and param-env
descriptor in a new crate, `vendor/rust-analyzer/crates/hir-ty-instance-desc` (encoding, and rebinding into a
database under the exact `BuildSpec`), helpers that load the closure of the homes they serve, and
grants routed by closure. Files: `harness/dist/homes.py`, `hir-ty-instance-desc`,
`crates/hir-ty-codegen/{mod,home}.rs`, `crates/ra-jit/src/main.rs` (load of a sub-graph),
`crates/blaze-dist`. Depends: D1; D4's first measurement naming the executors' front ends as the
limiter; C4 (keys of scripts and macros, so a helper does not rerun every script); P0-C.
Acceptance: the inventory, then, if built, smaller helpers on the CLI. From the reviews:
- [rb-M2] "Before choosing helper partitions, measure counts/codegen CPU/critical-path work and
  peak memory per actual home/required closure on CLI, including workspace ADTs and closures passed
  into dependency generics. D2 must demonstrate that useful CLI work (not just nushell's lowest
  third) fits eligible same-platform fleet helpers." Checked by: `homes.py` reports, per actual
  home (`Homes::home`'s answer) on `tracedecay-cli`, the functions, codegen CPU, critical-path share
  and the peak memory of that home's closure; the unit stops after the inventory if no closure
  that fits a fleet helper holds a useful share, and says so.
- [rb-B2] "Specify a versioned, lossless instance + param-environment descriptor ... Add
  independent-DB round-trip/compile tests (different interning order, long nested types, closures,
  async/coroutines, const generics, caller bounds and repeated crate units)" Checked by: each case
  encoded in one database and rebound in another with a different interning order compiles to an
  entry with the same digest; a type spelled past 1,024 bytes travels as a blob and round-trips.

Verify: `python3 -I harness/dist/homes.py --td ~/work/tracedecay && cargo test -p
hir-ty-instance-desc && python3 -I harness/dist/shards.py --closure`. Where: Mac; Linux for the CLI's closures.

**D3 The front-end image.** Goal: #274 for the dependency layer: the front end's results for
dependency crates laid out to be mapped read-only and validated by root, published by trusted
builders under `FrontEndKey` and dependency sources, mapped by executors. Kill criterion as
roadmap.md section 5's: a part whose mapping and validation costs more than rebuilding it stays
rebuilt. Files: a new crate under `vendor/rust-analyzer/crates/` for the image of front-end state,
`crates/ra-jit/src/main.rs`, `crates/blaze-dist`. Depends: D1. Acceptance: nushell's warm start from
the image against from the store, 3 runs each, same session; two executors on one machine mapping
one image hold less together than two loading it (`RssAnon`, Linux); the image is mapped
read-only, and a write through an executor's mapping fails (a test of the mapping code; this is
the mapping's flags, not isolation against job code). Verify: `python3 -I
harness/warmstart.py --image && python3 -I harness/dist/executors.py --image --linux-docker`.
Where: Mac; Docker.

**D4 Gate GD.** Goal: section 4's gate, measured and acted on. `harness/dist/measure.py` runs
GD's scenarios at 1, 2 and 3 executors, interleaved, n ≥ 3, with spread and limiter; if a bar
fails, the unit names the limiter, applies the next change of section 4's list (or files it as the
next unit), and measures again. Files: `harness/dist/measure.py`, `harness/dist/scenarios.toml`,
`docs/distributed.md` (results). Depends: D1, P0-H; D3 for the scenarios that start executors.
Acceptance: GD's correctness bar on every run; the speed bar met, or the change list's next step
taken and measured. From the reviews:
- [rb-M4] "Measure same-session push-to-verdict/build wall time and the complete critical-path
  timeline for cold first PR, subsequent body edit, signature edit and identity upgrade, including
  all executor preparation/import/link work; retain a separate codegen-stage bar only as
  diagnostic." Checked by: `measure.py` reports `Queued` to `Done` for each of those four scenarios
  with every stage on the critical path, the speed bar is judged on that time, and the
  compile-phase ratios are printed as diagnostics only.

Verify: `python3 -I harness/dist/measure.py --gate GD --runs 3` (Mac and Docker on fixtures;
`--fleet` writes the owner's run form). Where: Mac, Docker, Linux; fleet by the owner.

### Phase 4: tests

**T1 The cacheability contract.** Goal: section 6's contract, which is Bazel's
`cache_test_results=auto` and no more (the owner's parity rule, 2026-10-26, which narrowed this
unit from revision 3's classifier, audit mode and sampling). A pass is reused while the test, its
declared inputs (`data` files and `env` variables of `blaze-tests.toml`) and the run configuration
(`--test-threads`) are unchanged; `no_cache` and `external` opt a test out; a failed run keeps
nothing; a test that fails is retried up to its attempts (`flaky` in the manifest,
`RA_JIT_FLAKY_TEST_ATTEMPTS`) and reported `Flaky`. Files: `crates/ra-jit/src/cargo/affected.rs`,
`crates/ra-jit/src/cargo/run.rs`, `harness/dist/fixtures/tests`, `harness/dist/tests.py`. Depends:
none. The no-rustc audit (P0-A) remains pending: the fixtures have not run under it, and the
harness says so. Acceptance: a fixture workspace in which an unchanged test is skipped
on a second run; a changed test, a changed declared data file, a changed declared variable and a
changed `--test-threads` or declared executable mode each rerun; a test opted out with
`no_cache` and with `external` runs every time; a test that fails is not skipped at the next run,
nor after its code goes back to what
passed; a test that fails once is retried, reported `Flaky`, and cached, and with one attempt it
fails the run. The complete parity gate also includes Bazel repeated-run requests
(`--runs_per_test` above one) bypassing the cache; the current local harness has not measured
that part. From the reviews:
- [b-B2] "libc getpid/uname or listed child programs can observe unkeyed machine state; deny
  unknown effects instead of assuming libc is closed." Closed by the owner's decision, not by
  code: Bazel's key has the same blind spots (research 7.10) and blaze holds to Bazel's. What
  the unit checks instead is what Bazel gives the owner against them: the opt-out marker (a test
  that reads the clock, `getpid` or a default map's seed and fails now and then is marked
  `no_cache` or `flaky`) and the rerun of every failure.

Verify: `python3 -I harness/testselect.py && python3 -I harness/dist/tests.py --contract`. Where:
Mac; the sandbox steps on Linux (Docker), **not run** for this unit.

**T2 Passes across machines.** Goal: `TestKey` and `InputManifest` of declared files (the bundle's
digests, modes and lengths; nothing recorded by running), logical paths, the server's results
index and lookup, validation on B; `tests-reach` and `tests-passed` out of the target directory;
`CARGO_BIN_EXE_*` by content. Files: `crates/ra-jit/src/cargo/affected.rs`, `crates/ra-jit-store`,
`crates/blaze-serve/src/results.rs`, `harness/dist/crossmachine.py`. Depends: T1, C5.
Acceptance: a test that passed on A is skipped on B, which never ran it; changing a declared
file's content or mode on B, an argument, or a declared environment variable makes B run it; a
test marked `no-cache` runs on B; a failure on A is never served to B. From the reviews: [b-B2]
(the metadata and error-kind observations of a recorded read set) is accepted at parity
(appendix A).

Verify: `python3 -I harness/dist/crossmachine.py --tests`. Where: Docker; Linux.

**T3 Bundles and shards.** Goal: `RuntimeBundle`s built by the lead and materialised on helpers;
shards by recorded duration; tracedecay's flake policy (fail) kept. Files: `crates/blaze-dist`,
`crates/blaze-serve/src/jobs.rs`, `crates/ra-jit/src/cargo/affected.rs`. Depends: T2, E3.
Acceptance: a helper runs a test that needs a generated `OUT_DIR` file and a support tool not on
the host's `PATH`; a test that uses a tool its bundle does not declare, on a helper that does not have the tool,
fails naming it (a tool the helper happens to have is not policed, as on NativeLink's workers); a
deliberately flaky test is reported FLAKY and fails the job under the `flake-policy` default, as
nextest's `flaky-result = "fail"` would. Verify:
`python3 -I harness/dist/tests.py --bundles`. Where: Mac, Docker.

### Phase 5: the switch in tracedecay's CI

**S1 The CI client.** Goal: what a tracedecay workflow calls: join the tailnet with the Tailscale
action as today, run `cargo blaze test` with `RA_JIT_DIST` and `LocalPolicy=never`, publish nothing
itself, print the stage timestamps; a fork run, which has no tailnet, builds with no remote and
never calls the server. No token fetch and no `POST /v1/auth` at parity (section 2's access model).
Files: `harness/dist/ci/blaze-ci.sh`, `harness/dist/ci/README.md`. Depends: E3, T3, P0-H on both
platforms. Acceptance: against a local server, a pull-request run and a `master` run submit their
jobs and print the stage timestamps; a run with no tailnet address (a fork's) makes no call to the
server and builds with no remote. Verify: `bash harness/dist/ci/blaze-ci.sh --self-test`. Where:
Mac.

**S2 The test inventory and comparison.** Goal: review B M10. `harness/dist/ci/inventory.py`
lists every Rust test target, feature variant, custom harness, doctest and generated fixture
tracedecay's Bazel runs, from its BUILD files, and the tests that run rustc or cargo;
`compare.py` fails on a test missing, extra or not executed on either side, and on a different
verdict. Files: `harness/dist/ci/inventory.py`, `harness/dist/ci/compare.py`. Depends: none.
Acceptance: the inventory of `d79de82273` checked in for the owner to approve; compare fails on
a synthetic run that drops a test or adds one. From the reviews:
- [a-m2] "S2 verify only self-tests comparison, not inventory generation." Checked by: the verify
  command regenerates the inventory from `d79de82273`'s BUILD files and fails if it differs from
  the checked-in one.
- [b-M10] "preserving Bazel timeout/retry/resource-group policy is not specified and mandatory
  executed-on-both-sides gates contradict deliberate cached skips and excluded compiler-fixture
  tests." Checked by: the inventory records each test's timeout, retries and resource group as
  Bazel runs it, and blaze's job runs each test under them; compare accepts a skip only with its
  validated manifest, requires each test the inventory keeps on Bazel (the `rustc-tests` default)
  executed on Bazel and absent from blaze, and fails a synthetic run for each violation.

Verify: `python3 -I harness/dist/ci/inventory.py --check d79de82273 && python3 -I
harness/dist/ci/compare.py --self-test`. Where: Mac.

**S3 Shadow-mode report.** Goal: G2, G3, GB, GD and GS as the owner measured them, written
here with runs and spread, and a recommendation per owner step. Files: `docs/distributed.md`,
`docs/reports/DIST-SHADOW.md`. Depends: S1, S2, D4, and the owner's runs. Acceptance: each gate's
table filled from the owner's runs, or marked not run. From the reviews:
- [a-M2] "several retained rows still omit machine/session/commit, run count or named limiter
  instead of explicitly marking every missing datum unavailable." Checked by: every number S3
  writes carries machine, session, commit, run count, spread and limiter, or says which is not
  stated; `compare.py --provenance` fails on a table row that lacks one without saying so.

Verify: `python3 -I harness/dist/ci/compare.py BAZEL_TEST_LOGS BLAZE_EVENTS --inventory INVENTORY
&& python3 -I harness/dist/ci/compare.py --provenance docs/reports/DIST-SHADOW.md`.
Where: fleet by the owner.

**S4 The compatibility lane for tests that run rustc or cargo (only if the owner chooses it at
the `rustc-tests` gate).** Goal: section 6's second option: `cargo-blaze` as `cargo` and blaze's
answerer, extended to check a file and print rustc-compatible diagnostics, as `rustc` on those
tests' `PATH`, in their bundle by digest. Files: `crates/ra-jit/src/bin/ra-jit-rustc.rs`,
`crates/ra-jit/src/answers/own.rs`, `harness/dist/ci/inventory.py`. Depends: P0-A, T3. Acceptance:
tracedecay's three tests run on a blaze executor with no exec of a real
rustc or cargo (the shims are blaze's); each passes, or is named as
staying on Bazel with the diagnostic it could not match. Verify: `python3 -I harness/dist/tests.py
--compat`. Where: Mac.

### Owner steps, outside the units

These change tracedecay's repository or the fleet, and are the owner's, each after the gate
named:

1. Add `#![recursion_limit = "256"]` to `tracedecay-code-index-runtime` (before P0.8).
2. Grant a Linux host for P0-G (the `linux-host` gate).
3. Deploy `ra-jit serve` and agents on the fleet, on the tailnet only; add a Tailscale grant that
   lets `tag:bazel-ci` reach the server's API port on the coordinator host and nothing else, and
   give fleet nodes their own tag (the access model, section 2); set Bazel's reserve `B` (after
   E4).
4. Add the `rust-blaze` workflow in shadow mode (after S1).
5. Make it blocking (after G2, G3, GB, GD and GT).
6. Remove `linux-test`'s fleet shard and the macOS Rust tests from Bazel; narrow
   `tag:bazel-ci`'s grant to what Bazel still uses (after GS). Rollback: re-enable the removed
   Bazel targets and mark the blaze job non-blocking; nothing on Bazel's side is deleted until
   GS has held for two more weeks.

### Dependencies outside this plan

- **#344, per-item keys** (blaze-next): unchanged functions keep their keys across an edit. Until
  it lands, G2's bar is the affected closure, and signature edits are GD's best case.
- **#147, scripts and macros in the sandbox with read recording**: C4 keys by it on Linux, where
  blaze's existing sandbox records reads; on macOS nothing records them, so impure scripts and
  macros run again there. This is #147's existing sandbox, not the enforcement the scope ruling
  drops.
- **#271, the Merkle VFS**: `SourceRef::Root` replaces commits; an executor takes a job as a
  transaction instead of a checkout.

## Open questions for the owner

Each is a gate of the program with a default that holds until the owner answers. This design cites
them; it does not decide them.

| Gate | Question | Default until answered | Where it bites |
|---|---|---|---|
| `linux-host` | SSH to Ubuntu-main or the 64 GB desktop, or a larger Docker VM, for Linux at tracedecay scale? | none: Linux checks stay small, in the 8 GB Docker VM | P0-G, P0.8, D2's inventory |
| `bazel-reserve` | how much of Ubuntu-main stays with Bazel and NativeLink | measure first: 32 cores and 64 GB reserved | section 4's memory table (`B` = 64 GB), E4 |
| `pr-isolation` | user and sandbox per pull-request executor, or microVMs? | **withdrawn by the scope ruling** (owner, 2026-10-26): none; parity runs jobs in a work directory as NativeLink workers do | section 2 (record), E1, E2 |
| `rustc-tests` | tracedecay's tests that run rustc or cargo: keep on Bazel, run on blaze executors with the toolchain declared as under Bazel, blaze's compatibility lane, or change the tests? | keep on Bazel | section 6, S2's inventory, GS; S4 only if chosen |
| `flake-policy` | keep nextest's "flaky fails" for the blaze job? | keep | section 6, T3 |
| `rules-rust-js` | does Rust built for JS and Python tests stay on rules_rust? | yes | section 10 |
| `td-recursion` | add `#![recursion_limit = "256"]` to `tracedecay-code-index-runtime`? | the owner applies it; blaze runs patch it in throwaway worktrees until then | every tracedecay run |

Not yet gates, asked here: if the tracedecay worker cannot get under 16 GB on macOS (P0-C), should
verification of the full CLI move to a Linux host, or the guard rise for those runs? What are the
two 12-core machines' operating system and architecture, and is the 16 GB laptop a worker at all?
May P0.3 run its 2G and 512M cases on the Mac (an exception to the guard's fixed settings), or do
they stay in Docker (this design's default)?

## Not checked

- No part of this was built or run for this revision. Every bar is a target.
- Linux: nothing of tracedecay has run on Linux under blaze.
- The fleet's sizes are owner-reported, not measured; the Linux worker's architecture, NativeLink's
  version and configuration, and the hosted runners' sizes were not read.
- Whether blaze's codegen gives byte-identical entries for the same key on two machines (C5), and
  whether two processes with different interning orders name every root and instance alike (D0).
  Section 4's first stage rests on the second.
- Seatbelt on macOS 26 (c3-B1.3), **out of scope (owner, 2026-10-26)**: the sandbox and kernel
  enforcement it checked is not built. One narrow probe was **verified** before the ruling
  (u2-check3, macOS 26.6.2, arm64, one run): a profile a process applied to itself with
  `sandbox_init` was inherited by a child and a grandchild for `process-exec` and
  `file-read-data`, and a second `sandbox_init` could not remove it. Nothing else of the
  boundary was checked, and nothing now needs to be: no unit tests the production allow-list,
  `file-map-executable`, user namespaces, `noexec` mounts, seccomp user notification,
  `memfd_noexec` or Landlock.
- Whether two tracedecay front ends fit under the guard's 24 GB at once (GP-full's capacity
  preflight, D0), and how many functions tracedecay's whole build compiles (LOADMEM stopped at
  640,000). No run has measured `-p tracedecay --lib` as two processes, and nothing has measured
  the retained findings, export queues or test children (c3-D0.2); LOADMEM's single-run numbers
  give illustrative arithmetic, not a bound.
- Whether Linux AArch64 runs the whole build and `cargo blaze` (support.md: `cargo blaze` there is
  unverified); D0's preflight settles it for the Docker VM here, and until it passes a Docker
  success is **unverified** (c3-D0.1).
- How many functions the exporter leaves `Local` in a whole build, and so how long the lead's tail
  is, and how many raw entries are export-invalidated by a `Local` callee (D0); and whether the
  owned requests a helper retains for names it found survive the revision change with which
  u7's batch releases its memos (salsa's interned values), as section 4's retention contract
  needs, or must be found again (D0 tests it, c3-B2.2).
- tracedecay was read at `d79de82273`; its CI may have moved since.

## Appendix A: review findings

Revision 3 answers the two re-reviews of revision 2 (`2fd75c16`), both of 2026-10-26. Re-review A
gave each of the 25 findings of the first reviews a verdict and found seven new problems (here
`ra-N1` to `ra-N7`); re-review B reviewed the design anew (`rb-`). Revision 2's own mapping of the
first reviews is in `2fd75c16`'s appendix A. Revision 3.1 answers one further check, of revision 3
itself ([Check 3](#check-3-the-focused-check-of-revision-3), below).

Statuses: **resolved** (re-review A found it resolved at revision 2; nothing changed); **fixed in
doc** (this revision changes the design; where); **moved to acceptance** (the unit's acceptance
line, tagged with the id, quotes the finding and says how it is checked, so the code settles it);
**owner gate** (the owner's decision, with its default). **accepted at parity (owner scope ruling)** (revision 3.2: the finding's
resolution depended on a piece the owner's ruling of 2026-10-26 drops, so no unit settles it, and
the risk it named is accepted as equal to today's Bazel with NativeLink setup; the one line says
what is dropped and what stays; the earlier status follows it). A row can be fixed in the doc and carry
an acceptance line too. A finding fixed in the doc with no acceptance line is one no code can
check (a dependency rule, an arithmetic or a provenance label in this document).

### Re-review A: the 25 findings of the first reviews

| Finding | Re-review A | Revision 3 | Where |
|---|---|---|---|
| a-B1, spawn gate over job descendants | partly | **accepted at parity (owner scope ruling, 2026-10-26)**: the descendant-execution part needed the process-tree boundary and is dropped; what stays is blaze's own spawns (P0.2 [a-B1], parity remainder); earlier status: **fixed in doc**: a process-tree boundary replaces the spawn gate; **moved to acceptance**: P0.2 [a-B1] | section 0, "The no-rustc boundary"; P0.2 |
| a-B2, PR and Main isolation | resolved | **accepted at parity (owner scope ruling, 2026-10-26)**: its isolation (a user and sandbox per executor, E1, E2) is dropped; pull-request code runs on shared workers as it does under NativeLink today, an accepted risk equal to today's; earlier status: resolved | section 2; E1, E2 |
| a-M1, fleet sizes and queue | resolved | resolved | "What tracedecay runs today"; sections 5, 9 |
| a-M2, provenance of retained numbers | partly | **fixed in doc**: "n not stated" marks every quoted number without a run count; the nushell rows; **moved to acceptance**: S3 [a-M2] | header; "Numbers kept from revision 1"; S3 |
| a-M3, Phase 0 ownership and gating | partly | **fixed in doc**: no Phase 0 unit depends on P0-H or on the predicate it remediates (with ra-N3) | Phase plan preamble; section 11 |
| a-M4, wire schema | partly | **fixed in doc** in part: credentials and capability transport, `ops` bits, `JobContent` bytes, the work calls; **moved to acceptance**: C1 [a-M4] for every endpoint's and enum variant's payload struct | "Data shapes"; section 8; C1 |
| a-M5, remote test keys | resolved | **accepted at parity (owner scope ruling, 2026-10-26)**: the remote-test key was recorded under the stricter contract; parity keys by declared inputs as Bazel does (T1, T2 as reduced); earlier status: resolved | section 6; T1, T2 |
| a-M6, job identities and subscribers | partly | **moved to acceptance**: E3 [a-M6]; the job request's deadline, policy and test timeout are the subscriber's | section 8; E3 |
| a-M7, codegen dataflow | partly | **fixed in doc**: the lead's store-hydrated assembly keeps patch tracking; **moved to acceptance**: D0 [a-M7] | section 4, "Export and ingestion"; D0 |
| a-M8, where gates run | resolved | resolved | section 11 |
| a-M9, rollout and the GC's root producer | partly | **moved to acceptance**: E3 [a-M9], the lead writes its `RootManifest` | E3 |
| a-m1, 760 packages | resolved | resolved | "What tracedecay runs today" |
| a-m2, verify commands | partly | **moved to acceptance**: P0.7 [a-m2] and S2 [a-m2], with their verify commands changed | P0.7; S2 |
| b-B1, signing oracle | resolved | **accepted at parity (owner scope ruling, 2026-10-26)**: the signing oracle was closed by capabilities and signed records, which are dropped; the server signs nothing at parity and the tailnet's ACL decides who writes; earlier status: resolved | section 2 |
| b-B2, test cache inputs | partly | **accepted at parity (owner scope ruling, 2026-10-26)**: unknown effects always-run (T1) and observed metadata and errno (T2) belong to the stricter contract and are dropped; Bazel keys by declared inputs with the same blind spots, and the `no-cache` marker is the remedy; earlier status: **moved to acceptance**: T1 [b-B2] (unknown effects always run), T2 [b-B2] (metadata and errno observed) | T1; T2 |
| b-M1, distribution, not relocation | resolved | resolved | sections 4, 5 |
| b-M2, slot memory | resolved | resolved | sections 0, 4, 5 |
| b-M3, OIDC claims and forks | resolved | **accepted at parity (owner scope ruling, 2026-10-26)**: the OIDC claims and GitHub lookups are dropped; the fork rule stays, as today: fork runs get no tailnet and build with no remote; earlier status: resolved | section 2; C6 |
| b-M4, cargo semantics on the wire | partly | **fixed in doc**: `UnitSpec.host`, a per-unit `profile`, `ProfileSpec.backend` and `mir_opt`; **moved to acceptance**: C2 [b-M4] | "Data shapes"; C2 |
| b-M5, cross-machine reuse and platform | partly | **moved to acceptance**: C2 [b-M5] (native tool digests), C5 [b-M5] (platform fields in the entry key, another host) | C2; C5 |
| b-M6, read-set index and bundles | resolved | **accepted at parity (owner scope ruling, 2026-10-26)**: the read-set index (recorded reads) is dropped with the contract; runtime bundles by digest stay (T3); earlier status: resolved | section 6; T2, T3 |
| b-M7, completeness and liveness | partly | **fixed in doc**: the `Local` outcome, outcomes that are not verdicts, one fencing rule; **moved to acceptance**: D0 [b-M7], D1 [b-M7] | section 4, "The protocol"; D0; D1 |
| b-M8, tests that run rustc; nightly | regressed | **fixed in doc** as an **owner gate**: `rustc-tests`, default "keep on Bazel", with the compatibility lane (S4) as an option; the descendant-exec audit is P0.2's [rb-B1] | section 6; section 10; open questions; S2; S4 |
| b-M9, GC roots | not resolved | **fixed in doc**: GC by the entries builds used, compaction with a remap, retention by `ConfigKey`; **moved to acceptance**: C7 [b-M9] | section 3, "GC"; "Data shapes"; C7 |
| b-M10, CI switch coverage | partly | **fixed in doc**: GS and section 10 count validated skips and Bazel-kept tests, with Bazel's timeouts, retries and resource groups; **moved to acceptance**: S2 [b-M10] | sections 10, 11; S2 |

### Re-review A: new problems

| Finding | Severity | Revision 3 | Where |
|---|---|---|---|
| ra-N1, retention keyed by a changing source; no remap | major | **fixed in doc**: `ConfigKey`; the remap is compaction's commit point; **moved to acceptance**: C7 [ra-N1] | section 3; "Data shapes"; C7 |
| ra-N2, authentication bootstrap | major | **accepted at parity (owner scope ruling, 2026-10-26)**: there is no capability or agent-signature credential to bootstrap; C6's parity remainder is the tailnet's access model; earlier status: **fixed in doc**: `auth` exempt, capability and agent-signature headers; **moved to acceptance**: C6 [ra-N2] | section 8; C6 |
| ra-N3, Phase 0 gate cycle | major | **fixed in doc**: Phase 0 units never depend on P0-H or their own predicate | Phase plan preamble; section 11 |
| ra-N4, the watchdog is not the stated guard | major | **fixed in doc**: the guard's three rules over the whole tree; no lowered bounds on the Mac; **moved to acceptance**: P0.1 [ra-N4], P0.3 [ra-N4] | "Three decisions"; P0-B; P0.1; P0.3 |
| ra-N5, batch outcomes and fencing disagree | major | **fixed in doc**: a result of a dead lease generation is refused whole; no outcome is a verdict; **moved to acceptance**: D1 [ra-N5] | section 4, "The protocol"; D1 |
| ra-N6, overlap contradicts the seed order | major | **fixed in doc**: executors join with their own `RootSet`; no lead seed; **moved to acceptance**: D1 [ra-N6] | section 4, "The protocol"; D1 |
| ra-N7, timeout identity and unit order | minor | **fixed in doc**: the test timeout is the job request's, not `RunSpec`'s; a total canonical unit order | "Data shapes"; section 8 |

### Re-review B

| Finding | Severity | Revision 3 | Where |
|---|---|---|---|
| rb-B1, the no-rustc boundary is not a process tree | blocker | **accepted at parity (owner scope ruling, 2026-10-26)**: the process-tree boundary is dropped; blaze's own spawn rule stays (P0.2 [rb-B1], parity remainder: blaze's own spawns, not a job's descendants); earlier status: **fixed in doc**: the boundary (Linux mount namespace and kernel-enforced exec, macOS Seatbelt allow-list, fail closed), the spawn API, the answerer, negative fixtures; **narrowed in revision 3.1** (c3-B1.1): it claims no more than blaze never delegating Rust compilation, no host toolchain exposed and only approved code run, and no longer that a compiler denylist stops job-supplied compilers; **moved to acceptance**: P0.2 [rb-B1], [c3-B1.1], [c3-B1.2] | section 0; "The rules this design keeps"; P0.2 |
| rb-B2, a work item cannot rebuild an instance | blocker | **fixed in doc**: claimed discovery over identical front ends, nothing of an instance on the wire; the lossless descriptor as a second stage; **moved to acceptance**: D0 [rb-B2] (names alike across interning orders), D2 [rb-B2] (the descriptor's round trip); refined in revision 3.1 (c3-B2.1 to c3-B2.3) | section 4, "Moving work between processes"; D0; D2 |
| rb-B3, no portable entries; warm assembly does not consume them | blocker | **fixed in doc**: the exporter, the store-hydrated lead, the compile-query count; **moved to acceptance**: D0 [rb-B3]; refined in revision 3.1 (c3-B3.1 to c3-B3.3) | section 4, "Export and ingestion"; D0 |
| rb-M1, cache hits do not expand coverage | major | **fixed in doc**: `FromStore` is a usable closure in pinned packs; found names commit with their finder; **moved to acceptance**: D0 [rb-M1] | section 4, "The protocol"; D0 |
| rb-M2, dependency-only helpers | major | **fixed in doc**: home is not the defining crate; the first stage holds whole front ends; GD's memory step; **moved to acceptance**: D2 [rb-M2] | "What blaze has today"; section 4; D2 |
| rb-M3, lease expiry and fencing | major | **fixed in doc**: the fencing table, a progress timeout, at-least-once compute with exactly-once accounting, pins from upload; **moved to acceptance**: D1 [rb-M3] | section 4, "The protocol"; section 3; D1 |
| rb-M4, overlap and GD's hidden cost | major | **fixed in doc**: no lead seed; GD's bar is end to end; the identity scenario counts its front ends; **moved to acceptance**: D4 [rb-M4] | section 4; D4 |
| rb-M5, P0-A audits execution, not provenance | major | **accepted at parity (owner scope ruling, 2026-10-26)**: in part: the provenance audit stays for blaze's own run (P0.2 [rb-M5]); the denial of reads of rustc-built libraries by the boundary is dropped; earlier status: **fixed in doc**: P0-A's positive and provenance bars; **moved to acceptance**: P0.2 [rb-M5] | section 0; P0.2 |
| rb-M6, aggregate memory and executor trees | major | **fixed in doc**: P0-B is aggregate and per tree; section 5 enforces on the tree; **moved to acceptance**: P0.3 [rb-M6], E4 [rb-M6] | section 0; section 5; P0.3; E4 |
| rb-M7, eager refusals fail the job | major | **fixed in doc**: the seam is the build's own loop; a failure is speculative and the lead decides; **moved to acceptance**: D0 [rb-M7] | section 4; D0 |
| rb-N1, the 20% headroom not used | minor | **fixed in doc**: 70 GB, two executors at B = 64 GB | section 4, "Memory per machine" |
| rb-N2, three denominators for P0-D | minor | **fixed in doc**: 30 of 30; **moved to acceptance**: P0.5 [rb-N2] | section 0; P0.5 |

### Check 3: the focused check of revision 3

One check of revision 3 (`9875c09b`) by GPT 6.1, `u2-check3`, of 2026-10-26, limited to B1, B2, B3
and D0. Verdicts: B1 not workable as stated; B2, B3 and D0 workable with fixes. This is the last
edit pass before the design is accepted, so each fix is concrete and testable; there is no review
cycle after it. Statuses: **fixed in doc** (revision 3.1 changes the design or its statement;
where), **moved to unit acceptance** (the unit's acceptance line, tagged with the id, checks it).
A row can be both.

| Finding | Check 3 | Revision 3.1 | Unit | Where |
|---|---|---|---|---|
| c3-B1.1, the denylist admits compilers not in the host manifest | B1, not workable | **accepted at parity (owner scope ruling, 2026-10-26)**: the boundary, the compiler fixture table and the narrowed guarantee are dropped with it; no unit checks them; earlier status: **fixed in doc**: the claim narrowed to what is enforceable (blaze never delegates Rust compilation; no host toolchain exposed; approved code only, deny by default); in-process and brought-in compilers stated out of scope; the denylist kept as a diagnostic; **moved to unit acceptance**: the fixture table with expected outcomes | P0.2 [c3-B1.1] | section 0; P0-A; P0.2 |
| c3-B1.2, approve then `CONTINUE` | B1 | **accepted at parity (owner scope ruling, 2026-10-26)**: kernel-authorised exec, the exec store, Landlock, the racing, fd-based, compat-ABI and privilege tests are dropped; no unit checks them; earlier status: **fixed in doc**: exec authorised by the kernel over `noexec` mutable mounts, an exec store, Landlock, no exec-able memfd; `execveat`/`fexecve`, scripts and interpreters explicit; unknown ABIs killed, unresolvable targets denied; `no_new_privs` and dropped capabilities; **moved to unit acceptance**: racing replacement, fd-based exec, compat-ABI, unresolvable target, mutable-storage tests | P0.2 [c3-B1.2] | section 0, Linux steps 1 to 7; P0.2 |
| c3-B1.3, Seatbelt inheritance on macOS 26 | B1, not the blocker | **accepted at parity (owner scope ruling, 2026-10-26)**: the Seatbelt profile and its production checks are dropped; the one narrow probe stays recorded in "Not checked"; earlier status: **fixed in doc**: marked verified for one narrow probe only; the production profile, `file-map-executable`, audit delivery, pre-opened descriptors, mapped images, service delegation and later releases stay to test, the profile installed before acquisition, fail closed; **moved to unit acceptance** | P0.2 [c3-B1.2] (macOS counterparts), [c3-B1.1] | section 0 macOS; section 2; "Not checked"; P0.2 |
| c3-B2.1, a name is not a total, environment-safe identity | B2 | **fixed in doc**: `WorkKey` (unit, stable name, environment fingerprint) and `Edge` for unnameable requests, `tuple` digests, a conflict rule, a total root order, the same distinction for store keys; **moved to unit acceptance** | D0 [c3-B2.1]; C5 [c3-B2.1] | "Data shapes"; section 4, "Moving work between processes"; D0; C5 |
| c3-B2.2, finder retention | B2 | **fixed in doc**: lifetime, identity-pinning and lease contract; `Lost`; returned ranges and names; lead-loss row; **moved to unit acceptance** | D0 [c3-B2.2] | section 4, "Finder retention", "The protocol" steps 2, 5, 9; D0 |
| c3-B2.3, root agreement is an admission check, not a liveness proof | B2 | **fixed in doc**: three agreements distinguished; `inputs` digest and shared impure outputs; the lead's `RootSet` authoritative, a disagreeing early helper fenced; progress timeout from the grant and retry bound; liveness stated as termination with a result or a named failure, the deadline not a `Done`; **moved to unit acceptance**: sans-IO transition tests in D0, HTTP faults stay D1 | D0 [c3-B2.3]; D1 [rb-M3] | section 4, "The protocol" steps 1, 9, 10; D0 |
| c3-B2, digest-only on-demand grants (refinement) | B2 | **fixed in doc**: a pre-index of every eligible non-generic and on-demand function under the pinned `BuildSpec`; an unknown digest is `Lost { unknown-name }`, a conflict and a lead-compiled item, never missing work | D0 [c3-B2 pre-index] | section 4, "How (b′) works"; D0 |
| c3-B2, exactly-once (refinement) | B2 | **fixed in doc**: compute is at least once after a holder dies, accounting exactly once; no gate asks for exactly-once compute | D0, D4 (duplicate compiles as a diagnostic) | section 4, step 9 |
| c3-B3.1, `ExportContext` must resolve every request | B3 | **fixed in doc**: an immutable per-build policy and a pure per-request classification, `TypeId` from the request, debug and tracking made before memos drop, unknown is `Local`, one classifier for object-backed and batch-backed callers; **moved to unit acceptance** | D0 [c3-B3.1] | section 4, "Export and ingestion"; D0 |
| c3-B3.2, "0 exported names compiled" against the Local closure rule | B3 | **fixed in doc**: raw, accepted and export-invalidated sets; `accepted_closure` walks `Named` and `Root` edges; Local and missing outcomes propagate to ancestors before `Done`; the counted set stated; **moved to unit acceptance** | D0 [c3-B3.2] | section 4, "Export and ingestion"; GP's bar; D0 |
| c3-B3.3, seal and import | B3 | **fixed in doc**: seal a writer barrier with a durable finish result; import a cache generation boundary with a fresh `Entries` and a fresh full object; **moved to unit acceptance** | D0 [c3-B3.3] | section 4, "Export and ingestion"; D0 |
| c3-B3, what a batch end lacks (refinement) | B3 | **fixed in doc**: the three missing things (final-build policy, request-level metadata, durable writer completion) and the unit supplying each | P0.3 (sink seam), D0 | section 4, "Export and ingestion" |
| c3-D0.1, platform builds, Docker preflight, stage readiness | D0 | **fixed in doc**: `BIN_MAC` and `BIN_LINUX_AARCH64`, `proto.py --platform --stage`, the preflight failing closed, Docker as small-fixture only, GP-fixtures and GP-full with named statuses, C1 and C2 as dependencies, three recorded runs; **moved to unit acceptance** | D0 [c3-D0.1] | section 4, "The first unit"; phase plan; section 11; D0 |
| c3-D0.2, two-process capacity against the 24 GB guard | D0 | **fixed in doc**: an aggregate-capacity preflight for the exact BuildSpec under one watchdog and lock; illustrative arithmetic labelled; capacity failure keeps GP pending and changes the mechanism, never the guard; **moved to unit acceptance** | D0 [c3-D0.2] | section 4, "The first unit"; D0; "Not checked" |
| c3-D0.3, count from before load; isolated baseline; non-vacuous oracle | D0 | **fixed in doc**: trace from before load across every compile path, ungranted queries forbidden, phase counts apart, a cold private archived baseline, a separate verification arm, an origin table with a nonempty helper share; **moved to unit acceptance** | D0 [c3-D0.3] | section 4, "GP's bar"; D0 |
| c3-D0.4, the gate falsifies the mechanism, not the deliverable | D0 | **fixed in doc**: a failed GP blocks D1 and changes how (standing order 13); no claim for all Rust, platforms or in-process compilers; no timing claim; **moved to unit acceptance**: five injected faults fail the gate | D0 [c3-D0.4] | section 4, "What a failed GP falsifies"; D0 |
| c3-D0 prerequisites (refinement) | D0 | **fixed in doc**: D0's tracedecay stage is GP-full and needs P0-H on macOS plus the capacity preflight; D0 lands on GP-fixtures with GP-full `pending`; D1 needs both | D0; D1 | section 4, "The first unit"; D0; D1 |

### Counts

- Re-review A's 25: 10 resolved and unchanged; 9 fixed in the doc (a-B1, a-M2, a-M3, a-M4 in part,
  a-M7, b-M4, b-M7, b-M9, b-M10), 8 of them with an acceptance line too; 5 moved to acceptance
  only (a-M6, a-M9, a-m2, b-B2, b-M5); 1 an owner gate stated in the doc (b-M8). The one not
  resolved (b-M9) and the one regressed (b-M8) are fixed in the doc.
- Re-review A's 7 new problems: 7 fixed in the doc; 5 with acceptance lines (N1, N2, N4, N5, N6).
- Re-review B's 12: 12 fixed in the doc, the 3 blockers included; 11 with acceptance lines (all but
  N1).
- In all, 44 findings: 10 resolved earlier, 28 fixed in the doc, 5 moved to acceptance only, 1
  owner gate; 36 tagged acceptance lines in 20 units.
- Check 3's 13 findings (B1.1 to B1.3, B2.1 to B2.3, B3.1 to B3.3, D0.1 to D0.4) and its 4
  refinements (digest-only grants, exactly-once, what a batch end lacks, D0's prerequisites) are 17
  rows, all fixed in the doc. 14 tagged acceptance lines in 3 units (P0.2: 2, D0: 11, C5: 1) check
  14 of the rows; the other three (exactly-once, what a batch end lacks, D0's prerequisites) are
  statements this document settles, each exercised by a line above. B1.3 is a verification note,
  not a defect: its probe is recorded and the rest is P0.2's.
- Revision 3.2 (the scope ruling) changes 13 rows to "accepted at parity": a-B1, a-B2, a-M5, b-B1,
  b-B2, b-M3, b-M6, ra-N2, rb-B1, rb-M5 (in part), c3-B1.1, c3-B1.2 and c3-B1.3. The counts above
  are those of revisions 3 and 3.1 and are not recomputed. Tagged acceptance lines removed:
  P0.2 [c3-B1.1] and [c3-B1.2], T1 [b-B2], T2 [b-B2]; reduced to their parity remainder: P0.2
  [rb-B1], [a-B1] and [rb-M5], and C6 (with [ra-N2] noted as accepted). Findings whose units are
  kept whole (the D0 to D4, C1 to C5, C7, S1 to S3 lines) are unchanged.

## Appendix B: the research edits

[research/bazel-nativelink.md](research/bazel-nativelink.md) "What this changes in
docs/distributed.md":

| # | Edit | Applied in |
|---|---|---|
| 1 | Trust: NativeLink mixes `master` and pull requests; `Main` only from slots that never ran pull-request code; disposable pull-request state | "What tracedecay runs today"; section 2. Out of scope (owner, 2026-10-26): parity keeps NativeLink's mixing as an accepted risk |
| 2 | Job identity split; cancel when the last subscriber leaves | "Data shapes"; section 8; E3 |
| 3 | Admission by reservations, `Main` first, stage timestamps, no CPU or memory in keys | section 5; E4 |
| 4 | Tests: Bazel's precedent and limits, always-run, audit, failures never cached, read set with the verdict, runtime bundle, flake exemption the owner's | section 6; T1 to T3. Revision 3.2 keeps Bazel's precedent, the opt-out marker, failures never cached and the runtime bundle; always-run classification, audit and the recorded read set are out of scope (owner, 2026-10-26) |
| 5 | GC by record generation, completeness, leases | section 3; C7 |
| 6 | Epoch in signed records; forced rebuild of one crate | section 2 "Records, epochs and repair". Out of scope (owner, 2026-10-26) |
| 7 | `cpu_count` in keys, one instance `main`, client uploads, plaintext gRPC, 5.2 GB inputs | "What tracedecay runs today" |
| 8 | The 200 s pole **unverified**; the run was a release build | section 9 |
| 9 | REAPI: the verdict and reasons of 7.9 | section 3 |
| 10 | A resolved build spec and a native-toolchain digest | "Data shapes"; section 1; C2 |
| 11 | Racing: a first-finishing failure fails; `Never` on hosted runners with tracedecay's reason | section 4 "Failure" |
| 12 | Mode D: no prior system splits one crate; gated | section 4. Changed by the owner's decision: distributed compile is required; the gate decides how, not whether |
