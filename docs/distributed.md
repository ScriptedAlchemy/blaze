# Distributed cache, compile and tests

2026-10-26, revision 2. Status: design, nothing built. Written against the fork's `dist/main` at
`a67ea8db` (code as at `c8e5f6a8`; `dist/main` has gained docs and one harness since) and
tracedecay at `d79de82273`. Each claim is marked **read** (in a source we read, or measured by
us), **reported** (a document or someone says so, not re-checked here) or **unverified** (our
inference or an estimate). Numbers name the file they come from. Code paths are still `ra-jit`
where the rename to blaze has not reached them.

Revision 1 (`45449ae6`) was rejected by both reviews of [PR
#1](https://github.com/ScriptedAlchemy/blaze/pull/1): four blockers and nineteen majors. This
revision answers each of them; [appendix A](#appendix-a-review-findings) maps every finding to what
changed and where. It also takes in three things that did not exist when revision 1 was written: the
measurement of blaze on tracedecay ([reports/TRACEDECAY.md](reports/TRACEDECAY.md), G0), the survey
of Bazel with NativeLink ([research/bazel-nativelink.md](research/bazel-nativelink.md), whose twelve
edits are applied, see [appendix B](#appendix-b-the-research-edits)) and the memory survey
([research/memory.md](research/memory.md)).

The goal: blaze gives tracedecay a distributed cache, distributed compile and distributed tests
for Rust on Linux x86-64 and macOS arm64, good enough that tracedecay drops NativeLink for its
Rust build and test actions there. Bazel keeps Windows, JS, Python, Clippy and the release and
dist (thin LTO) builds. Workers run blaze, never rustc ([no-rustc.md](no-rustc.md)).

Three decisions of the program's owner bind this design:

- **Distributed compile is required.** It means one build's compile work split across machines,
  not only whole jobs moved onto a fleet machine. A gate that fails changes how it is done, never
  whether it ships. Revision 1's "mode D is deleted if G4 fails" is gone.
- **Memory is guarded.** Every blaze run on the development Mac sets `RA_JIT_MEMORY_MAX=16G`,
  `RA_JIT_OWN_BASE_MEMORY=12G` and `RA_JIT_SPARE=0`, under a watchdog that kills the ra-jit tree
  past 24 GB of summed footprint or under 3 GB of free swap.
- **Heavy runs are serialised** by one lock on the development Mac; no dev host is reachable from
  it. Which gates can run where is part of each gate ([section 11](#11-gates)).

## Short version

- **Phase 0 comes first: blaze builds tracedecay.** G0 measured a no-go on macOS arm64: the
  whole build (no cargo) fails on a writable static of the standard library (#255); 18 of 18
  edits went to cargo because the CLI's build script prints something else on a rerun; a loaded
  tracedecay worker reached 58.3 GB on a 48 GiB Mac; `RA_JIT_MEMORY_MAX` does not bound a busy or
  last-asked worker (`daemon.rs:96-104`). Linux is unmeasured. Eight units fix and re-measure
  that before any distributed code depends on it, each closing a measurable predicate.
- **rustc is refused before it is spawned.** Every process blaze starts goes through one gate that
  refuses rustc, rustdoc and a compiling cargo with a named reason; a worker runs with it closed.
  An audit with stand-in tools counts attempted runs. A log line is no longer the check.
- **Trust has three roles.** Trusted `Main` builders that never run pull-request code and are the
  only principals that publish to `Main`; per-pull-request executors created from trusted
  read-only inputs, isolated by user and sandbox, kept for that pull request's pushes only and
  destroyed after; a control plane whose credentials no job can read. Capabilities are short,
  bound to a job, a namespace and a source commit the server verified through GitHub's API, not
  through claims GitHub does not issue. Signed records carry an epoch.
- **Distributed compile splits codegen into batches over a shared work set.** blaze already
  compiles ahead from a queue of roots and generic instances that compiled functions discover
  (`compile_unnamed`, `hir-ty-codegen/mod.rs:2565`). That queue moves to the coordinator. Work
  items are grouped by home crate (`home.rs`), so a helper needs only the front end of its
  crates' dependency closure, and its results are content-keyed store entries any machine can
  link. The lead assembles the object from the store. No system we read splits one crate's
  compile across machines (Bazel, NativeLink, sccache-dist, buck2 all distribute whole rustc
  invocations); this is new and its gate decides how, not whether.
- **The front end is what costs.** In u1's one tracedecay load, loading, resolving and expanding
  took 120 to 210 s and compiling all 77,345 functions ahead 40.7 s (**read**, TRACEDECAY.md,
  one run each, contended). So a helper pays only if its front end is warm or shared: trusted
  helpers are warm on `master`, a pull request's executors stay warm for its later pushes, and a
  front-end image of the dependency layer (#274) is mapped read-only instead of loaded.
- **Tests are cached only when every input is declared or recorded.** A test that reaches time,
  randomness (including `HashMap`'s seed), threads, sockets, unlisted processes or unlisted
  foreign code always runs. Failures are never cached. An audit mode runs everything uncached and
  revokes caching for any test whose cached pass it contradicts. tracedecay's flake policy
  (fail) is kept.
- **Admission reserves measured memory and CPU** per executor and per job, `Main` first, a bounded
  queue, and stage timestamps on every job so a queue always has a named limiter. CPU and memory
  never enter a key. Disk is bounded on every tier, each with a default, a command and a check.
- **The wire request carries a resolved `BuildSpec`**: the unit graph after cargo's
  configuration and feature resolution, the profile, flags, environment, working directory, the
  run options of a test, and a digest of the native toolchain. A request blaze cannot honour is
  refused before it is scheduled.
- **Plan.** Twenty-nine units in six phases: Phase 0 (blaze builds tracedecay), the wire and the
  cache, trust and execution, distributed compile, tests, the CI switch. Each lands alone with a
  goal, files, acceptance and one verify command that checks all of it. Owner-operated steps in
  tracedecay's own CI are listed apart from the units.

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
  of" the crates its own name mentions, so `Vec<u8>::push` does not depend on the workspace crate
  that asked for it. This is what makes a compiled instance portable between builds, and what
  section 4 partitions by.
- **The remote** (`crates/ra-jit/src/remote.rs`, `remote/`): `Dir`, `S3` and `Https` places;
  verified transfers with resume (`transfer.rs`); Ed25519-signed manifests with turns
  (`signed.rs`); remote GC by the clock (`collect.rs`, at least two hours' grace); a keeper that
  pushes and pulls in the background every five minutes and never makes a build wait
  (`keeper.rs:1-24`).

### What a build schedules

- One daemon compiles "a set of root functions into one object" (`CodegenUnit::compile`,
  `mod.rs:636`), each function a salsa query whose result names its callees by stable name
  (`fragment.rs`), on every core (`SymbolResolver::in_parallel`, `mod.rs:388`). A callee that
  cannot be compiled poisons its callers and the build restarts with it rejected
  (`compile_or_hold`, `:656`).
- **Compiling ahead** (`compile_unnamed`, `mod.rs:2565-2622`) is a shared queue: threads pop a
  root or an instance, compile it (`compiled_root`, `compiled_instance`), and push every generic
  callee the result names, with its home crate, unless already seen. "A build that knows them
  finds it compiled." This loop is the seam section 4 distributes.
- `RA_JIT_UNITS=1` groups functions into LLVM modules for whole-program builds; it is not the
  default, as it builds no faster (`units.rs:1-45`).
- One daemon stops scaling: compiling the standard library is 4.5 s on 16 cores and 3.9 s on 96,
  a fifth of the samples in rust-analyzer's interning and the lock slow paths beside it
  ([no-rustc.md](no-rustc.md), "The standard library, shipped"; dev-aws-8; run count not stated).

### Numbers kept from revision 1, with their provenance

Review A M2 asked every retained number to carry its machine, run count, spread and whether
cargo or rustc did part of it. These are background, not baselines: no ratio below is a claim of
this design.

| Number | Source | Machine, runs, spread | Hybrid? |
|---|---|---|---|
| ripgrep cold 16.8 to 17.4 s with an empty store, 3.98 to 3.99 s warm store, 4.01 to 4.02 s warm remote; nushell 78.9 s, 31.3 s, 31.5 s; helix 87.0 to 89.7 s, 25.5 to 25.7 s, 26.0 to 26.3 s | no-rustc.md, "The cold start without rustc" | dev-aws-8, 96 cores pinned; ranges over the runs recorded, run count not stated | no: "every crate ra-jit's and linked by ra-jit" |
| of nushell's 31 s warm: resolving 3.8 s, asking the store about roots 3 s, writing the object 7 s, listing 4.7 s, link 1 s | same | same; one breakdown | no |
| a fresh machine building cargo 0.92.0: 152.4 s no remote, 117.1 s with packs, 94.4 s with packs and cargo's units; lazy 100.2 s | [reports/REMOTE.md](reports/REMOTE.md) | loopback MinIO, one run each, no spread | **yes**: cargo ran rustc for dependencies and link lines, and three refused functions were compiled by rustc. Not all-blaze evidence |
| nushell body edit 258 ms; ready from nothing 52.1 s; 5.2 GB ready, 7.5 GB after its base, 11.2 GB peak | [roadmap.md](roadmap.md), [ci-runner.md](ci-runner.md) | as reported there; spread not given here | no |
| sandbox about 16 ms a process on Linux | [ci-runner.md](ci-runner.md):388 (354 ms for 20 runs against 26 ms bare) | one measurement, no spread | no |
| slot sizes on Linux x86-64, 16 cores pinned: ripgrep to axum 1.4 to 2.0 GB, helix 5.7, nushell 7.1, bevy 15.2, biome 19.3, bun 24.5 GB | [research/memory.md](research/memory.md), from benchmarks.md | per-project rows of one bench run | no |

Revision 1's "eager fetch beats lazy" (94.4 against 100.2 s) is withdrawn as a conclusion: one
run each, hybrid.

## The rules this design keeps

- **No rustc, enforced before spawn.** blaze today reaches cargo and rustc from several places:
  a cold build dispatches to cargo (`cargo/mod.rs:265-285`), `by` starts `cargo build` or
  `cargo test --no-run` (`cargo/state.rs:490-504`), the wrapper runs the compiler
  (`cargo/wrapper.rs:150-151`), a build script blaze cannot build is left to rustc. Each is a
  `no-rustc` bug. Phase 0 puts every `Command` blaze spawns behind one function that refuses
  rustc, rustdoc and a cargo subcommand that compiles, before the spawn, with a named error; in
  worker mode the refusal cannot be turned off, and refusal fails the job. A job is never turned
  green by cargo. `RA_JIT_RUSTC_BASE=0` does not disable cargo's dispatch (review A B1; it only
  stops patching a rustc-built base, `main.rs:3686-3695`), so revision 1's reliance on it is
  dropped.
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

A digest of a value is `sha256("blaze/" ‖ name ‖ "/v1" ‖ 0x00 ‖ canonical CBOR of the value)`,
where `name` is the type's name below. A signature is Ed25519 over the same bytes with `sig/` in
front of the name. Unit C1 fixes these with golden vectors.

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
struct ExecPlatform {               // 0..=5
    triple: String,                 // 0, the blaze binary's target: no cross builds
    os: String,                     // 1, "macos 26.6.2", "linux 6.8"
    sdk: Option<(String, String, String)>, // 2, macOS: SDK name, version, deployment target
    libc: Option<(String, String)>, // 3, Linux: name and version, or the declared sysroot
    linker: (String, String, Digest), // 4, what links: name, version, digest of its executable
    cpu: String,                    // 5, target CPU and features codegen assumes
}

/// Which tree a job builds: a commit the server verified (section 2), or #271's root.
enum SourceRef {
    Git { repo_id: u64, commit: [u8; 20], tree: [u8; 20] }, // 0
    Root { root: Digest },                                   // 1, BLAKE3 Merkle root
}

/// A build after cargo's configuration, workspace, feature and profile resolution: what
/// `cargo blaze <command>` would do, with nothing left to the worker's environment.
struct BuildSpec {                  // 0..=11
    source: SourceRef,              // 0
    lock: Digest,                   // 1, sha256 of Cargo.lock
    units: Vec<UnitSpec>,           // 2, the resolved unit graph, sorted by package id
    asked: Vec<u32>,                // 3, set: indexes of the units the command named
    profile: ProfileSpec,           // 4
    platform: Digest,               // 5, ExecPlatform's digest
    rustflags: Vec<String>,         // 6, resolved from RUSTFLAGS, CARGO_ENCODED_RUSTFLAGS
                                    //    and config, only flags blaze honours
    config_env: Vec<(String, String)>, // 7, set: [env] of .cargo/config.toml, resolved
    script_env: Vec<(String, String)>, // 8, set: variables build scripts may read, fixed
    cwd: String,                    // 9, relative to the workspace root
    command: Command,               // 10
    toolchain: StoreIdentity,       // 11
}

struct UnitSpec {                   // 0..=6
    package: (String, String, Digest), // 0, name, version, source id digest
    target: (u8, String),           // 1, kind (0 lib, 1 bin, 2 test, 3 example, 4 bench,
                                    //    5 build script, 6 proc macro) and name
    features: Vec<String>,          // 2, set: after unification
    cfg: Vec<String>,               // 3, set: every cfg the unit is built with
    edition: u16,                   // 4
    deps: Vec<(u32, String)>,       // 5, set: unit index and the name it is used under
    test: bool,                     // 6, cfg(test)
}

struct ProfileSpec {                // 0..=7
    name: String,                   // 0
    opt_level: u8,                  // 1, 0..=3; s and z refused until blaze has them
    debug: u8,                      // 2, 0 none, 1 line tables, 2 full
    debug_assertions: bool,         // 3
    overflow_checks: bool,          // 4
    panic: u8,                      // 5, 0 unwind, 1 abort
    strip: u8,                      // 6, 0 none, 1 debuginfo, 2 symbols
    split_debuginfo: u8,            // 7, 0 off, 1 packed, 2 unpacked
}                                   // lto other than off: refused (release stays on Bazel)

enum LocalPolicy { Never, Fallback, Race }  // 0, 1, 2

enum Command {
    Check,                          // 0
    Build,                          // 1
    Test(RunSpec),                  // 2
    Run(RunSpec),                   // 3
}

struct RunSpec {                    // 0..=9
    filters: Vec<String>,           // 0
    exact: bool,                    // 1
    skip: Vec<String>,              // 2
    ignored: u8,                    // 3, 0 not ignored, 1 only ignored, 2 include ignored
    no_run: bool,                   // 4
    no_fail_fast: bool,             // 5
    test_threads: Option<u32>,      // 6
    harness_args: Vec<String>,      // 7, after `--`; a custom harness gets exactly these
    env: Vec<(String, String)>,     // 8, set: the environment a test program is given
    timeout_ms: Option<u64>,        // 9
}

/// The part of a BuildSpec a warm front end depends on: what an executor or an image
/// must agree with to take a job as an edit. digest of { 0: units with their features, cfg,
/// edition and deps but not sources, 1: lock, 2: rustflags that set cfg, 3: profile's
/// debug_assertions and overflow_checks, 4: toolchain, 5: platform's triple }.
struct FrontEndKey(Digest);

/// The namespaces of the cache, and what may write each (section 2).
enum Namespace {
    Main,                           // 0, written only by trusted builders, at verified commits
    Pr(u64),                        // 1, written by that pull request's executors and runners
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
    signature: [u8; 64],            // 6, by the server, over fields 0..=5
}

struct PackRef {                    // 0..=4
    pack: Digest,                   // 0
    size: u64,                      // 1
    keys: Digest,                   // 2, sha256 of its key list
    packed: Option<(Digest, u64)>,  // 3, the .zst object, when sent compressed
    format: u32,                    // 4, store FORMAT
}

/// What one build used: the GC's live roots are record generations, not crate names.
struct RootManifest {               // 0..=4
    namespace: Namespace,           // 0
    source: SourceRef,              // 1
    spec: Digest,                   // 2, BuildSpec digest
    records: Vec<(CrateKey, u64)>,  // 3, set: crate and generation read or written
    tests: Vec<TestKey>,            // 4, set
}

/// A job's three identities (research 7.11, review A M6).
struct JobContent(Digest);          // BuildSpec digest plus namespace: what merges duplicates
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
    ops: u32,                       // 6, bit set: lookup, fetch, publish, events, cancel, roots
    not_after_ms: u64,              // 7, wall clock, at most the job's deadline
    epoch: u64,                     // 8
}

/// One unit of distributed codegen (section 4): roots or instances of one home crate.
struct WorkItem {                   // 0..=2
    name: String,                   // 0, the stable name (stable.rs), machine-independent
    env: Digest,                    // 1, digest of the param env and crate it is asked under
    home: CrateKey,                 // 2
}

struct Batch {                      // 0..=4
    execution: ExecutionId,         // 0
    id: u64,                        // 1
    lease: (Attempt, u64),          // 2, holder's attempt and lease end, ms
    home: CrateKey,                 // 3
    items: Vec<WorkItem>,           // 4
}

enum ItemOutcome {
    Compiled { entry: EntryKey },   // 0
    Refused { why: String },        // 1, a CodegenError: fails the job, never goes to rustc
    FromStore { entry: EntryKey },  // 2
}

struct BatchResult {                // 0..=4
    batch: u64,                     // 0
    lease: (Attempt, u64),          // 1
    outcomes: Vec<ItemOutcome>,     // 2, in the order of the batch's items
    found: Vec<WorkItem>,           // 3, set: instances the compiled code names, not yet asked
    packs: Vec<PackRef>,            // 4, sealed and uploaded before this is sent
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
| `roots/<ns>/` | `SourceRef` and spec | `RootManifest` | replaced per build of a commit |
| `epochs` | namespace | current epoch | by the operator only |
| `jobs`, `executors` | execution id, executor id | requests, leases, events, reservations | in memory, with a log |

## 0. Phase 0: blaze builds tracedecay

G0 says blaze does not build tracedecay without cargo on macOS, does not take its edits, and does
not hold its memory. Every phase after this one assumes all three. So Phase 0 is a track of its
own, first, with one predicate per blocker. Each predicate is measured with
`harness/tracedecay.py` (extended by unit P0.1), at least three runs per arm, guarded, and its
result is written into [reports/TRACEDECAY.md](reports/TRACEDECAY.md) by the unit that closes it.

| Predicate | Measured as | Bar | Where |
|---|---|---|---|
| **P0-A, no rustc** | the stand-in tools of `harness/norustc.py` on `PATH` and as `RUSTC`, `RUSTDOC`, `CARGO`, every blaze process in worker mode; the gate's own audit log | 0 runs of rustc or rustdoc and 0 compiling cargo subcommands attempted, on hello-world, ripgrep and `tracedecay-cli` build, edit and test; an unsupported project is refused before any spawn, naming the reason | this Mac; Linux at scale |
| **P0-B, memory bounded** | footprint (macOS) or `RssAnon` plus `RssFile` (Linux) of every worker, 1 s samples; the daemon's log | with `RA_JIT_MEMORY_MAX=M`, no worker passes 1.05 M for more than 2 s in any state (loading, busy, last asked); a worker stopped by the bound fails its build with the bound's message, and the build does not go to cargo; the harness watchdog never fires | this Mac |
| **P0-C, tracedecay fits** | the same samples through a cold load, then 20 edit/undo pairs (memory.md M1) | a loaded `tracedecay-cli` worker holds at most 16 GB footprint on macOS arm64 at ready and after 20 pairs, so a guarded run completes; Linux's number recorded beside it | this Mac; Linux at scale |
| **P0-D, edits are the daemon's** | `blaze status` and the build logs, 5 runs each of the three edit kinds of TRACEDECAY.md and their reverts | 18 of 18 edit builds `Finished ... [ra-jit]`, 0 `building with cargo`; the CLI's build script rerun when its watched inputs change and its new output used | this Mac |
| **P0-E, the whole build on macOS** | `BLAZE_WHOLE_BUILD=1` | hello-world, ripgrep and `tracedecay-cli` build and their programs print what cargo's do; 0 `without a rustc symbol`; `harness/doctestonce.py` passes on macOS (#255) | this Mac |
| **P0-F, a cold build's peak** | the watchdog's 2 s series | the transient of script compiles at most 4 GB footprint (16.2 GB today) | this Mac |
| **P0-G, Linux at scale** | P0-A to P0-F on Linux x86-64 | each bar met | a Linux host of at least 64 GB (owner's `linux-host` gate) |
| **P0-H, G0 again** | TRACEDECAY.md's scenarios: cold, edits, no-op, test, disk | blaze's cold whole build and each edit kind measured against cargo in the same session, n ≥ 3, limiter named. No speed bar: Phase 0 is about building, not winning | both platforms |

What is decided here, and why:

- **A loading worker is bounded by checking itself** (TRACEDECAY.md's first shape): the worker
  reads its own footprint while it loads, as the own-base thread does (`main.rs:9030-9050`), and
  past the ceiling stops with a named reason. `over()` is also changed so a worker alone over the
  ceiling is not exempt. In worker mode the stop fails the job; locally the daemon tells the
  user, as it does today, and the no-rustc gate decides what happens next. Unit P0.3.
- **The worker gets smaller by the ranked list of memory.md section 4**, in its order: reclaim at
  idle (type GC, solver caches, arena purge), recycle on growth, one worktree path per executor with
  gone checkouts unloaded, then what the program's unit u5 (a profile of the 58.3 GB load, running)
  names as the largest holder. P0-C's 16 GB is set by the guard: a worker larger than that cannot be
  verified on this Mac at all. If the owner grants a Linux host instead, P0-C keeps its bar on macOS
  (the Mac Studio has 64 GB, section 4) and adds Linux's. Unit P0.4.
- **A build script whose rerun prints something else is run, not refused.** blaze reruns a
  script into a second directory and refuses it if the output differs (`cargo/script.rs:670-688`).
  cargo instead runs the script when a watched input changed and takes its latest output. The
  CLI's script prints one `rerun-if-changed` per file git does not ignore
  (`tracedecay-cli/build.rs:417-422`), which an edit changes. The fix: a script whose output
  differs between two runs on the same inputs is marked impure, run once per build in the
  sandbox where there is one, and its latest output is used, as cargo does; its run is never
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
| build-script run keys (`cargo/script.rs` `run_key`) | hash the script's executable | C4: hash the package's content, its dependencies' ids, the environment cargo gives and, on Linux, the digests of what the sandbox recorded it reading. Impure scripts (P0.5) are never shared | D2 (a helper not running every script) |
| proc-macro expansion keys (`ra-jit-expand`) | hold cwd and environment | C4: checkout-relative cwd, the environment cargo gives, recorded reads | D2 |
| per-target-dir test records (`tests-reach`, `tests-passed`, `CARGO_BIN_EXE_*` by size and mtime, `affected.rs:213-228`) | yes | T2 | T2 |

**Rejected.** A tree hash as the cache key (an edit anywhere would miss everything; research
7.1). The lock file in function keys (dependency crate ids already carry name, version and
metadata). Cross-host identities that let Linux compile for macOS (not known to give the same
code; no phase needs it).

## 2. Trust

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
  (**unverified** on macOS 26). Until E2 lands, the Mac Studio runs trusted builders only, and a
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
  is one policy; revision 1's "forks get read-only claims" is withdrawn (review B M3). Unit C6 tests
  that a fork's run is refused.
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
  pin the key (`RA_JIT_REMOTE_TRUST`, `signed.rs`). The namespace is in the signed bytes, so a
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
  each in the namespaces the capability may read, `Pr(n)` before `Main`.
- **Completeness** (NativeLink's `completeness_checking`): a record is answered with only the
  packs the server holds; one that names a missing pack is answered without it and the pack's
  absence is logged.
- **An existence set** (`existence_cache`): `missing` is answered from memory.
- **Verification on write**: a blob is hashed before it is kept.
- **Success only**: only a job that finished publishes records; a failed job's packs are not
  named by any record and are collected.
- **Namespaces, capabilities and signing** (section 2), and **GC by live roots** (below).

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
entries from many builds. Each generation lists only the packs of the build that published it
plus the packs of the generation before that the build used; section 7's GC drops what no live
generation names (review B M9).

### GC

- A build's `RootManifest` names the record generations it read and wrote. Live roots: the last
  20 commits of `master` per `BuildSpec` and every root written in the last 14 days. A record
  generation named by no live root is dead; a pack named by no live generation is dead.
- **Compaction:** the server rewrites a crate's live entries into fresh packs when its live
  generations name packs that are mostly dead, publishes a new generation of the same content,
  and lets the old packs die. This is the step a crate-name root could not do.
- **Leases:** a running job's lookups pin the generations they answered until the job ends or
  its deadline passes; GC skips pinned generations. No eviction retry is needed (research, the
  adopt table's row 21).
- **Budget:** `--max` bytes with a low-water mark at 80%. Past `--max`, the server collects
  dead data, then live `Pr(n)` namespaces idle longest, then `Main` roots older than the last 20
  commits; it never collects a pinned generation. If live data alone passes `--max`, the server
  says so and stops accepting `Pr(n)` publishes before it evicts live `Main` data.
- **Scheduling:** collection runs in the server every hour and when a publish crosses the budget,
  not by hand. A blob no generation names is removed after at least two hours (`collect.rs`'s
  grace), checked again right before removal.
- `Pr(n)` is dropped by `DELETE /v1/ns/pr/<n>` from a `pull_request: closed` workflow or 14 days
  after its last write.
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

### The unit: codegen batches by home crate, over one work set

blaze already compiles ahead from a work set: roots, then every generic instance a compiled
function names, deduplicated, popped by any thread (`compile_unnamed`, `mod.rs:2565-2622`).
Three facts make that set distributable:

1. **A compiled function names what it calls by stable name** (`fragment.rs`), not by an address
   in one object. It compiles without its callees compiled. No worker waits on another to
   compile.
2. **An instance's code depends only on what its name mentions** (`home.rs`). Its home crate's
   dependency closure is all the front end it needs.
3. **A compiled function is a content-keyed store entry.** Any machine on the same identity that
   has the entry links it as if it compiled it ("a build that knows them finds it compiled").

So the unit of distribution is a **batch**: work items of one home crate, a few thousand
functions' worth, leased from a work set the coordinator holds for one execution. It is coarse
enough that a request costs nothing beside it (about 2 ms of thread time a function on nushell,
21.2 s on 16 cores for 165,752, roadmap.md, **read**), and fine enough to balance the long pole:
the `tracedecay` crate's roots and the instances homed in it are many batches, compiled on as
many executors as hold its closure.

| Unit | Verdict | Why |
|---|---|---|
| one function per request | rejected | a request and its round trip per 2 ms of work |
| one crate per action (Bazel's) | rejected as the unit, **kept as the partition key** | blaze has no per-crate action; but grouping work by home crate is what lets a helper load less than the whole workspace |
| static shards by stable-name hash (revision 1's mode D) | rejected | every shard needs the whole front end, a slow shard holds the build, and the reviews found no completion or liveness rule (review A M7, review B M7) |
| **batches by home crate, leased from one work set** | **chosen** | the existing ahead-compile loop, across processes; dynamic, so it balances; home-crate locality for packs and front ends |
| one big box | kept where it exists | Ubuntu-main is one (128 cores); its executors are part of the fleet, and helpers on it are the first step (gate GD) |

### The protocol

Roles in one execution: the **lead** (the executor that owns the job: it builds the program,
links it and runs the tests) and **helpers** (executors of the same trust that load a front end
for the same tree and take batches). The coordinator holds the work set.

1. **Seed.** The lead loads the front end for the job's `BuildSpec`, computes the roots
   `compile_unnamed` would start from, looks their crates up in the cache (one batch), and posts
   the roots whose entries the cache lacks as work items, grouped by home crate, to the
   coordinator. Items whose entries are in the store are marked `FromStore` without a lease.
2. **Lease.** An executor asks for a batch of the home crates it can serve (those whose closure
   its front end holds). The coordinator answers with a `Batch` and a lease (attempt, end); the
   items are marked leased. Leases last 60 s and are renewed by the holder's heartbeats every 5 s.
3. **Compile.** The holder compiles each item (`compiled_root`, `compiled_instance`), never
   waiting for another item. It collects the instances its results name (as `compile_unnamed`
   does) into `found`.
4. **Publish, then report.** The holder seals the batch's entries into a pack (`seal_open`),
   uploads it (`PUT /v1/blob`), and only then sends its `BatchResult`. The coordinator adds
   `found` items not yet seen to the work set under their home crates. A batch's result is
   terminal: every item is `Compiled`, `FromStore` or `Refused`.
5. **Refusal.** A `Refused` item fails the job with blaze's reason, naming the function. It never
   goes to rustc. (Locally a refusal restarts codegen with the function rejected, `mod.rs:636-645`;
   in a distributed build the coordinator does the same: it marks the item refused and the lead's
   assembly treats it as rejected, so the rejection rounds are the lead's, as today.)
6. **Done.** The compile phase is done when no item is pending or leased. Coverage is tracked per
   item, so "done" means every root and every found instance has a terminal outcome.
7. **Assemble.** The lead fetches every helper's pack and imports it into its store, synchronously,
   before it builds the object. Then it runs the normal build (`CodegenUnit::compile`), which finds
   every function in the store. What is not a stable-named function stays the lead's: statics,
   the allocator and runtime shims, vtables, drop glue reached only through them, and the
   rejection rounds (`mod.rs:800-945`). A missing entry is compiled by the lead then; the lead
   never waits on a helper for it.
8. **Fencing.** A lease that expires returns its items to the work set; a later result for that
   lease is accepted for items still not terminal (entries are content-keyed, so a late duplicate
   is harmless) and ignored for the rest; a late `Refused` from an expired lease is ignored and
   the item is compiled again. Every message carries execution id and attempt; a reassigned
   execution's old attempt is refused.
9. **Liveness.** No executor waits on another during compile; only assembly waits, and only for
   leased batches with live leases, which are returned on expiry. So the protocol cannot
   deadlock on a missing entry (review B M7). Helper reservation is non-blocking: a lead that
   finds no helper free compiles alone, and two leads never hold helpers waiting for each other.
10. **Cancellation and deadline.** The agent kills each executor's process group (section 2,
    rule 4); the coordinator drops the execution's work set and leases.

Placement: the work set's state machine is sans-IO, in a new crate `crates/blaze-dist`; the
hooks that take items from and give results to codegen go in `crates/hir-ty-codegen`
(`compile_unnamed`'s queue behind a trait); the IO driver in `crates/ra-jit/src/worker.rs`.
Nothing here is language semantics; the front-end image below is, and goes in
`vendor/rust-analyzer`.

### Where each executor's front end comes from

| Option | Cost, measured | Verdict |
|---|---|---|
| each helper loads the whole workspace cold | tracedecay on macOS: load 79.9 to 159.2 s, resolve 20.4 to 26.4 s, macros 19.85 to 23.73 s (2 runs), and 58.3 GB when ready (1 run) (TRACEDECAY.md) | rejected: it costs 3 to 5 times the 40.7 s the whole ahead compile took, and more memory than any fleet machine but one holds |
| **each helper loads only the closure of the home crates it serves**, from trusted inputs | `tracedecay-framing`'s closure: a 3.0 GB worker against the CLI's 58.3 GB (TRACEDECAY.md, **read**: "the load follows the closure of the package asked for"); load time **unverified** per closure | **chosen, first form** (unit D2). Helpers for lower crates load in a fraction of the time and compile while the lead is still loading: the front end of the lead and the codegen of the helpers overlap |
| **a warm front end**: a trusted builder warm on `master`, a pull-request executor warm on its pull request's last push | an edit is 258 ms on nushell (**read**); tracedecay's edit time as the daemon's is unmeasured until P0-D | **chosen** for executors that already exist: a job is an edit of what they loaded |
| **a front-end image of the dependency layer** (#274, #40): the front end's results for the dependency crates, laid out to be mapped read-only and validated by root, published by a trusted builder, keyed by `FrontEndKey` and the dependency sources | not built. #274's target: a warm restart under 1 s against 35.4 s on nushell (**reported**); its kill criterion is in roadmap.md section 5 | **chosen, second form** (unit D3): a new executor maps it instead of loading; executors on one machine share its pages; a pull request's executor starts warm without ever touching trusted state it could write |
| fork a warm trusted daemon | not measured | rejected (section 2) |
| ship MIR | no serialised MIR exists; codegen asks the database for layouts, vtables, type ids and symbols (no-rustc.md, **read**) | rejected |

Helpers are matched to batches by closure: the coordinator knows each unit's dependency closure
from the `BuildSpec`'s unit graph, and a helper whose front end holds closure C serves every home
crate whose closure is inside C. The `BuildSpec` carries the unified features and cfg of every
unit, so a helper loading a sub-graph builds each crate exactly as the lead does.

### Where it pays, and what is measured

The one tracedecay load measured spent 120 to 210 s in its front end and 40.7 s compiling every
function ahead (TRACEDECAY.md, one run each, contended). A cold helper cannot pay on that build.
Distributed compile pays where codegen is large and front ends are warm or small:

- **a new blaze identity** (a blaze upgrade): every function is compiled again, on every
  trusted builder warm on `master`;
- **a signature edit in a crate many depend on**: it renames every checkout crate downstream
  (cache.md), so their functions compile again until per-item keys (#344) land;
- **a lock-file change**: the dependency layer's instances compile again;
- **the first push of a pull request** whose executors start from the image;
- **a cold `Main` build of a new commit range** after the cache was disowned (an epoch bump).

Gate GD measures these on tracedecay, not on a body edit (which compiles a few functions).

### Memory per machine

A front end's size decides how many executors a machine holds. From fleet.md (**reported**) and the
numbers above, each executor reserving its size plus 20%; the arithmetic is **unverified** and is
replaced by measured reservations (section 5) once P0-C and P0-G have numbers.

| Machine | RAM | Held back | Whole tracedecay front end, today (58.3 GB, macOS, 1 run) | At P0-C's bar (16 GB) | Small closures (3 GB, like `tracedecay-framing`) |
|---|---|---|---|---|---|
| Ubuntu-main, 128 cores | 256 GB | 16 GB system, B GB for what stays on Bazel (owner to say) | (240 − B) / 64 ≈ 3 executors at B = 48 (the Linux size is unmeasured; macOS's footprint counts compressed pages Linux does not have) | (240 − B) / 20 ≈ 9 at B = 48 | many; CPU binds first: blaze stops scaling near 16 cores, so 8 executors of 16 cores is a starting partition |
| Mac Studio, M1 Ultra | 64 GB | 8 GB system | 0: 58.3 GB does not fit beside the system with headroom | 2 | about 15 by memory; its 20 cores bind first |
| 12-core desktop | 64 GB | 8 GB | 0 | 2 | about 15 by memory; its 12 cores bind first |
| 12-core laptop | 16 GB | 4 GB | 0 | 0 | 2 to 3: helpers for low crates and test shards only, if the owner makes it a worker |

The row that decides the most is Ubuntu-main: with P0-C met, one trusted builder and several
pull-request executors fit beside Bazel's remaining work. With the image (D3), executors on one
machine share the dependency layer's pages, and each adds only what its own crates and edits
dirty (**unverified** until D3 measures it).

### Gate GD, and what changes if it fails

**Measured** (unit D4): tracedecay `tracedecay-cli`, the five scenarios above, on 1 executor
(the lead alone), 2 executors on two machines, and 3 on three; trusted builders on `master`;
at least 3 runs per arm, interleaved, one session; stage timestamps and the work set's counters
(items, batches, leases expired, bytes moved, front-end load per helper).

**Correctness bar** (must hold in every run): the program's tests give the same verdicts as the
one-executor build; every entry key present in both has the same digest; 0 refused items that
the one-executor build compiled; 0 attempted rustc runs.

**Speed bar:** on the identity-change and signature-edit scenarios, 2 machines finish the
compile phase (stage `Loaded` to `Compiled` of the lead) at least 1.4 times faster than 1, and 3
at least 1.8 times. On a body edit, distribution must cost at most 5% over 1 executor (the lead
compiles small work alone when it is faster: the coordinator does not lease batches under a
size, set from this measurement).

**If the speed bar fails, the limiter named by the stage timestamps decides what changes**, in
this order, and GD is measured again:

1. **The coordinator or the network** (leases, uploads, imports dominate): larger batches, packs
   uploaded while compiling, the lead importing as packs arrive.
2. **The helpers' front ends** (helpers spend longer loading than compiling): helpers on the
   lead's machine first, with the image shared (D3) before helpers across machines; then
   helpers kept warm across jobs by `FrontEndKey`.
3. **Memory** (a helper cannot hold the closure it needs): partition finer, so helpers take only
   crates whose closure fits; shrink the worker further (P0.4's list, items 5 and 9 of memory.md).
4. **Codegen itself is small** next to load and link for a scenario: that scenario's builds lead
   alone, by the batch-size rule above, and GD's speed bar applies to the scenarios where it is
   not. The work set stays on for every build; only small builds lease nothing.

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

**Failure.** An executor that misses three heartbeats loses its leases and its job; the job is
reassigned once with a new attempt; a second loss fails it with the reason. `LocalPolicy`:
`Never` on hosted runners (tracedecay's own reason: "Fleet compilation must not fall back onto
the smaller hosted VM"), `Fallback` (build locally after the deadline or a remote failure),
`Race` (build locally from the start, per job, never per function). Under `Race` the first to
finish wins, but a first-finishing failure fails the job, as Bazel's dynamic execution does
(research 7.4), so local and remote differences surface. Locally, every policy goes through the
same no-rustc gate.

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
  macOS by blaze's thread count; memory by the executor's `RA_JIT_MEMORY_MAX` set to its
  reservation and, on Linux, the cgroup's `memory.max`.
- **Order.** `Main` before pull requests, then first in, first out. A pull request's next push
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
  x86-64 on Ubuntu-main and the Linux desktop; macOS arm64 on the Mac Studio. A pull request's
  macOS leg runs on a hosted macOS runner with `Main` reads until unit E2 isolates pull-request
  executors on the Mac Studio; its platform digest differs from the Mac Studio's (the SDK skew of
  `docs/ci-remote-builds.md:10-12`), so it shares function entries (same triple and identity) but
  not programs or test verdicts.
- **Gate GB, the burst** (owner-operated): K concurrent pull-request jobs, K = 1, 3, 6, of the
  same three edit kinds, against NativeLink's fleet build of the same commits, in one session,
  3 runs per arm; push-to-verdict p50 and p95, and each wait named by stage. Bar: p95 no worse
  than NativeLink's at K = 3 and 6, with Bazel's reserve held. This is what decides whether the
  fleet's capacity is enough, not G3's single-job latency (review B M1).

## 6. Distributed tests

### What is cached: the contract

A pass is reused only when everything that could change the verdict was declared or recorded
and is the same. Bazel caches passes under a declared-input key and limits the damage with an
`external` tag, `--runs_per_test`, and never caching failures (research 7.10); blaze's key is
recorded rather than declared, so its contract is stricter:

1. **Statically closed.** The test's reach (`RA_JIT_TESTS=affected`'s analysis) touches none of:
   time (`Instant`, `SystemTime`, `sleep`, timeouts), randomness (`getrandom`, `RandomState` and so
   every default `HashMap`/`HashSet`), threads and synchronisation with timeouts, sockets
   including loopback, process spawn other than the bundle's programs, foreign functions not in
   the platform's C library, environment reads outside `RunSpec.env`. Today the classifier leaves
   time, randomness, threads and locks out on purpose (`fncache.rs:315-318`); unit T1 adds them as
   reasons to always run, not as reasons to cache.
2. **Dynamically recorded.** On Linux the test ran in the sandbox, which recorded every path it
   looked up, including failed lookups, with content, mode, length and link targets, and it wrote
   only its temporary directory. The recorded set is its `InputManifest`. `/proc`, `/sys` and `/dev`
   are not recorded (`sandbox.rs:64-67`), so a test that reads them always runs.
3. **Not marked.** The owner's test manifest may mark tests always-run, like Bazel's `external`.
4. **On macOS** there is no recording until unit E2's sandbox exists: only tests that pass rule 1
   and read no file at all are cacheable; the rest always run.
5. **Failures are never cached.** A failure is reported and the next run runs the test.

Anything else always runs. How many of tracedecay's tests the contract caches is measured, not
assumed (gate GT): if `RandomState` alone makes most tests uncacheable, the next step is to seed
it per test in the sandbox from a digest of `TestBaseKey` (domain-separated, so the seed is not
part of what it keys), and to measure again. Time is not virtualised: tests that read the clock
always run.

### Finding a pass on a machine that did not run it

A fresh machine cannot compute a `TestKey` before it runs the test: the key holds what the run
read (review B M6). So:

1. The machine computes the `TestBaseKey` from what it has before running: the test program's
   digest, the test's name, the `RunSpec`, the platform, the sandbox policy and the
   `RuntimeBundle`'s digest.
2. It asks `POST /v1/results/lookup` with the base keys and the namespaces it may read, `Pr(n)`
   then `Main`. The answer lists, per base key, the passes kept with their manifests.
3. For each manifest it checks every observation against its own bundle and files: same digest,
   mode and length; absent paths still absent; links the same. Paths are logical (`$CHECKOUT`,
   `$OUT_DIR/<unit>`, `$BUNDLE`), resolved on each machine.
4. A manifest that holds in full makes the `TestKey`, which is the pass's. The test is skipped and
   the report says so: "skipped: passed under the same inputs at abc1234". If none holds, it runs.
5. A run publishes its manifest blob and, if it passed, the verdict, together.

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
  test does not fail" is withdrawn (review B M10).
- **Audit mode.** A scheduled run on `master` (daily) runs every test uncached and compares it
  with the cached verdicts of the same `TestBaseKey`s. A test whose cached pass the audit
  contradicts loses caching (it is marked always-run until a person clears it), and the report
  names it. Besides the daily run, 5% of cache hits in every job are run anyway, picked at random.

### Tests that run rustc or cargo

Three of tracedecay's tests run rustc or cargo and assert what they print (above). On a blaze
executor the no-rustc gate refuses those spawns, so these tests cannot pass there. They are
listed in the test inventory (unit S2) and stay on Bazel's runner until blaze offers a
rustc-compatible diagnostics front end on `PATH` that they accept, or the owner changes them.
Which of those is wanted is an open question for the owner, not a default of this design.

## 7. Disk, bounded on every tier

Each tier has a default, a command and a harness check (the owner's requirement, 2026-10-26).

| Tier | Today | Bound | Default | Command | Check |
|---|---|---|---|---|---|
| Local store | LRU by pack within `RA_JIT_CACHE_MAX`, a tenth of free disk, 256 MiB to 32 GiB (`ra-jit-store/src/lib.rs:89-96`), run automatically | unchanged | as today | `blaze store collect` (exists as `StoreDir::collect`; C7 gives it a command) | `harness/storeguards.py`, plus C7's identity case |
| Stale identity, local | packs of an old blaze identity stay until LRU reaches them | packs no record or root of the current identity names are collected first | on every collect | `blaze store collect --stale` | C7: upgrade the binary, collect, old identity's packs gone, the new identity's build compiles what it must |
| Target directory | unmanaged; blaze's is 23.0 to 27.3 GB against cargo's 14.2 GB on tracedecay (TRACEDECAY.md), up to 3.9 times cargo's elsewhere (biome 42.5 GB against 11.0, benchmarks.md) | blaze's own files in it (its objects, programs, `.ra-jit-*` records) within `BLAZE_TARGET_MAX`, oldest program first, never the running program's | 2 times the size cargo's own build leaves, measured at the first build | `blaze clean --keep-latest` | C7: twenty builds of moving commits stay within the bound; P0.7 explains the extra 8.9 to 13.1 GB first |
| Cache server | none (revision 1: GC by roots) | section 3's GC: `--max`, low-water 80%, roots by generation, leases, completeness, hourly | `--max` 70% of the server's disk | `ra-jit serve collect --dry-run` | C7: 30 `master` commits and 10 closed pull requests with new generic instances each, within budget, the last 20 roots hit, obsolete instances gone |
| Executors | — | a pull-request executor's directories are deleted with it; a trusted builder's worktree is one path, moved by edits, and its daemon unloads checkouts that are gone (memory.md item 3) | at executor end | `blaze-agent prune` | E1: after 10 pull requests opened and closed, no executor directory, user or process left |
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
- **Errors** are one shape on every call: `{0: code, 1: message, 2: retry_after_ms or null}`, with
  HTTP status from the code's class. Codes: 1 malformed (400), 2 version (400), 3 unauthenticated
  (401), 4 not allowed (403), 5 not found (404), 6 conflict, a generation or attempt moved (409),
  7 too large (413), 8 busy (429), 9 refused by blaze, with the reason (422), 10 internal (500).
- **Calls** (every request but `hello` carries a capability):

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
  | `POST /v1/jobs` | `{0: BuildSpec, 1: namespace, 2: deadline, ms since the epoch, 3: LocalPolicy}` | `{0: JobContent, 1: ExecutionId, 2: subscriber id}`; an equal `JobContent` running already is joined, not started again |
  | `GET /v1/jobs/<execution>/events?from=N` | | stream of `{0: sequence, 1: attempt, 2: event}`; `from` replays after a reconnect |
  | `DELETE /v1/jobs/<execution>/subscribers/<id>` | | `{}`; the execution is cancelled when its last subscriber leaves |
  | `POST /v1/workers/hello` | `{0: agent name, 1: capacity, 2: list of ExecutorState}` | `{0: accepted}` |
  | `POST /v1/workers/next` (long poll) | `{0: free reservations, 1: FrontEndKeys held}` | an assignment `{0: ExecutionId, 1: Attempt, 2: BuildSpec, 3: Capability}`, or a `Batch`, or nothing |
  | `POST /v1/work/lease` | `{0: ExecutionId, 1: home crates served, 2: Attempt}` | a `Batch` or nothing |
  | `POST /v1/work/result` | `BatchResult` | `{0: accepted items}` or error 6 |
  | `POST /v1/workers/events` | `{0: ExecutionId, 1: Attempt, 2: list of events}` | `{0: continue or cancel}`; heartbeats ride on it |

- **Joining from a hosted runner.** The Tailscale action with an OIDC client whose only scope is
  auth keys for `tag:blaze-ci`, which may reach only the server's port; an ephemeral node, logged
  out at the job's end. Then a GitHub OIDC token for `POST /v1/auth`. Two independent checks, as
  tracedecay does for Tailscale and its workflow condition.
- **Rejected.** gRPC on the hot path; REAPI as the wire (section 3); JSON on the wire; the
  daemon's JSON-line socket protocol over TCP (per user and local by design, `socket.rs`).

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
| tests that run rustc or cargo | section 6; until the owner decides |
| Rust built as an input of a JS or Python test | stays rules_rust until the owner settles it |

**How they coexist.** A `rust-blaze` job runs beside Bazel in shadow mode, then blocking, then
`linux-test`'s fleet shard and the macOS Rust tests leave Bazel. Each step is the owner's, in
tracedecay's repository, after a gate (section 11). The comparison is not "equal verdicts on the
tests both ran": it is against an inventory (unit S2) of every Rust test target, feature
variant, custom harness, doctest and generated fixture Bazel runs today, approved by the owner,
and it fails on any test missing, extra or not executed on either side, as well as on a
different verdict. Bazel keeps NativeLink for what stays on it, with a reserve on Ubuntu-main
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
| **P0-A to P0-H** | every unit after Phase 0 that names them | section 0 | section 0 | Mac; P0-G Linux |
| **G1, cross-machine hits** | C5's switch to per-crate records | `harness/dist/crossmachine.py`: machine B (other `HOME`, `CARGO_HOME`, checkout path, target and store) builds A's commit from A's published records | ripgrep and nushell: 0 functions compiled on B; tracedecay: at least 99% of functions from the store on B, every miss listed with its cause; programs print on B what cargo's builds print there, panic paths included | Mac (two machines on one host); Linux |
| **G2, the cache in CI** | S1's blocking step | hosted runner, tracedecay test programs from nothing: no remote, warm `Main`, and Bazel's fleet build of the same commit | warm `Main` no slower than Bazel's fleet build; functions compiled only in the affected closure of the commit's changes (the edited crates and the checkout crates downstream, cache.md), until #344 | fleet |
| **G3, one job** | E3's switch | a warm lead against Bazel's fleet job, same commits: body edit in a leaf crate, body edit in the `tracedecay` crate, signature edit in a crate many depend on | push to verdicts faster than Bazel's fleet job for each, 5 runs each | fleet |
| **GB, the burst** | S3's blocking step | section 5 | section 5 | fleet |
| **GD, distributed compile** | D4 closes; S3's blocking step | section 4 | section 4; a failure changes how, by section 4's list | Mac and Docker (two executors, small fixtures); Linux; fleet (tracedecay) |
| **GT, tests** | T2's switch to skipping | tracedecay's suite: share of tests cacheable under the contract; then 20 runs of an unchanged commit with audits; then adversarial changes (a fixture's mode, a removed file, an environment variable, a clock-reading test, a child tool) | 0 cached passes the uncached audit contradicts; every adversarial change runs the affected test; the share cacheable recorded | Mac; Linux; fleet for the suite |
| **G6, requests per build** | C5 | server counters | one lookup request per build; 0 per-function requests; server CPU per job recorded | Mac |
| **GS, shadow** | the owner's removal of Bazel's Rust paths | two weeks of shadow mode on `master` and pull requests, against S2's inventory | every inventoried test executed on both sides with equal verdicts, or each difference filed and fixed; 0 missing, extra or unexecuted | fleet |

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

Fixture repositories and commits are pinned in `harness/dist/fixtures.toml` and fetched by
`python3 harness/dist/fetch.py` (unit P0.1), into `~/work/<name>` at the pinned commit. Paths
under `harness/dist/`, the crates `blaze-wire`, `blaze-serve` and `blaze-dist`,
`crates/ra-jit/src/{spawn,worker}.rs`, `crates/ra-jit/src/remote/serve.rs` and
`docs/reports/DIST-SHADOW.md` are new, made by the unit that first names them; every other path
exists at `c8e5f6a8`. "Depends" names units and the Phase 0 predicates a unit needs met; a unit
whose acceptance runs tracedecay needs P0-H on that platform.

### Phase 0: blaze builds tracedecay

**P0.1 The tracedecay harness, guarded and audited.** Goal: one harness for every Phase 0
predicate on both platforms. Extends `harness/tracedecay.py` with: the guard as a default it
cannot turn off; `norustc.py`'s stand-in rustc, rustdoc and cargo on `PATH` and as `RUSTC`,
`RUSTDOC`, `CARGO`, counting every run with its argv; a `--predicate P0-x` mode that runs that
predicate's scenarios and exits non-zero unless its bar holds; `fixtures.toml` and `fetch.py`.
Files: `harness/tracedecay.py`, `harness/dist/fixtures.toml`, `harness/dist/fetch.py`,
`harness/norustc.py` (the stand-ins as a module). Depends: none. Acceptance: on hello-world, the
audit counts the rustc runs today's `blaze build` makes (more than 0) and `--predicate P0-A` fails
naming them; the watchdog kills a run at a 512 MB limit and records it. Verify:
`python3 -I harness/tracedecay.py --td ~/work/hello --predicate P0-A --expect-fail && python3 -I harness/tracedecay.py --self-test`.
Where: Mac.

**P0.2 rustc refused before spawn.** Goal: P0-A's mechanism. Every process blaze starts goes through
`crates/ra-jit/src/spawn.rs`, which refuses rustc and rustdoc with any arguments and cargo's
`build`, `check`, `test`, `run`, `rustc` and `doc`, recognised by the executable it resolves to and
not only its name, with a named error and an audit line, when `BLAZE_NO_RUSTC=deny`; worker mode
sets it and cannot unset it. A unit test fails if any `Command::new` outside `spawn.rs` remains in
`crates/ra-jit`. Files: `crates/ra-jit/src/spawn.rs`, `cargo/mod.rs`, `cargo/state.rs`,
`cargo/wrapper.rs`, `cargo/own_script.rs`, `cargo/script.rs`, `main.rs`. blaze already answers
rustc's questions (`-vV`, `--print`) itself (`crates/ra-jit/src/answers/own.rs`); a question it
cannot answer is refused like any run. Depends: P0.1. Acceptance: with deny, a project blaze cannot
build without cargo fails before any spawn, naming the reason; the audit of P0.1 counts 0 compiler
runs on hello-world on Linux (Docker) where the whole build works. Verify:
`cargo test -p ra-jit spawn && python3 -I harness/tracedecay.py --td ~/work/hello --predicate P0-A --linux-docker`.
Where: Mac, Docker.

**P0.3 A loading worker is bounded.** Goal: P0-B. The worker checks its own footprint during
load and its build, and past `RA_JIT_MEMORY_MAX` stops with "the worker holds N MB, over the N MB
of RA_JIT_MEMORY_MAX, while loading"; `over()` (`cargo/daemon.rs:96-104`) no longer exempts a
worker alone over the ceiling; in worker mode the stop fails the job. Files:
`crates/ra-jit/src/cargo/daemon.rs`, `crates/ra-jit/src/main.rs` (load path), `memory.rs`.
Depends: P0.1. Acceptance: nushell with `RA_JIT_MEMORY_MAX=2G` and ripgrep with `512M`: the worker
stops within 2 s of passing 1.05 times the bound, the build says why, the watchdog never fires;
with the bound above the worker's size nothing changes. Verify:
`python3 -I harness/tracedecay.py --td ~/work/nushell --predicate P0-B --memory-max 2G && python3 -I harness/tracedecay.py --td ~/work/ripgrep --predicate P0-B --memory-max 512M`.
Where: Mac.

**P0.4 A tracedecay worker under 16 GB.** Goal: P0-C. In memory.md section 4's order, stopping when
P0-C holds: reclaim at idle in `let_go` (type GC, solver caches on every thread, arena purge; the
program's unit u6, running), recycle on growth, one worktree path per worker and unload of gone
checkouts, then the largest holders unit u5's profile of the tracedecay load names. Each step lands
as its own commit with its measured effect. Files: `crates/ra-jit/src/main.rs`, `memory.rs`,
`cargo/daemon.rs`, and what u5 names (in `vendor/rust-analyzer` for front-end holders). Depends:
P0.3, the program's unit u5's report. Acceptance: P0-C's bar on macOS, 3 runs; each step's effect in
TRACEDECAY.md with spread. Verify: `python3 -I harness/tracedecay.py --predicate P0-C --runs 3`.
Where: Mac. If 16 GB is not reached, the unit lands what it reached and the owner decides between
the Linux host and a higher bar (open questions).

**P0.5 Build scripts whose rerun differs.** Goal: P0-D. A script whose second run prints or
writes something else is marked impure instead of refused: run once per build (in the sandbox on
Linux, recording its reads), its latest output used, as cargo does; never shared across
machines. Files: `crates/ra-jit/src/cargo/script.rs` (`rerun_into`, `run_key`),
`crates/blaze-build-script`. Depends: P0.1. Acceptance: a fixture script that prints a list of
files git tracks: an edit is the daemon's, the list updated; tracedecay's three edit kinds 15 of
15 the daemon's, 0 `building with cargo`. Verify:
`python3 -I harness/scripts.py --impure && python3 -I harness/tracedecay.py --predicate P0-D --runs 5`.
Where: Mac.

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
`cargo/daemon.rs`. Depends: P0.6. Acceptance: P0-F's bar on tracedecay; a table of the target
directory by producer in TRACEDECAY.md. Verify:
`python3 -I harness/coldprof.py --memory ~/work/ripgrep && python3 -I harness/tracedecay.py --predicate P0-F`.
Where: Mac.

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
duplicate keys, floats, tags, unknown keys and oversize items, naming each. Verify:
`cargo test -p blaze-wire`. Where: Mac.

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
name; whether the deployment target changes a function's code is recorded. Verify:
`cargo test -p blaze-wire && python3 -I harness/dist/buildspec.py`. Where: Mac.

**C3 The server, today's layout.** Goal: `ra-jit serve --cache DIR --listen ADDR` with `hello`,
blobs with `Range`, `missing` from an existence set, verification on write, and today's
`manifests/`; a client place `Place::Serve` (`blaze+http://host:port`). Push, pull, lazy and the
keeper work through it unchanged. Files: `crates/blaze-serve`, `crates/ra-jit/src/remote.rs`,
`crates/ra-jit/src/remote/serve.rs`, `harness/remote.py`. Depends: C1. Acceptance: `remote.py`'s
round trip, damage, signing and kill checks pass against the server as a third leg. Verify:
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
answered without it. Verify: `python3 -I harness/dist/crossmachine.py --all --remote server`. Where:
Mac.

**C6 Trust: identities, capabilities, namespaces.** Goal: section 2's server side. OIDC
verification against a configured JWKS, the GitHub API lookups, capabilities, roles, `Main` and
`Pr(n)`, epochs, the forced rebuild, fork refusal. `harness/dist/github.py` is a fake issuer and
API. Files: `crates/blaze-serve/src/auth.rs`, `policy.rs`, `github.rs`,
`crates/ra-jit/src/remote/signed.rs`, `harness/dist/github.py`, `harness/dist/auth.py`.
Depends: C5. Acceptance: a pull request's token cannot publish `Main` or `Pr(other)`; a `Main`
token with a `SourceRef` not on `master` is refused; a wrong workflow, audience, issuer, expired,
replayed (`jti`) or unsigned token is refused; a fork's run is refused; an enrolled agent cannot
publish without a capability or cancel another's job; a record of an old epoch is refused after a
bump; a `Pr` record replayed as `Main` is refused. Verify:
`cargo test -p blaze-serve && python3 -I harness/dist/auth.py`. Where: Mac.

**C7 GC and disk on every tier.** Goal: sections 3 and 7. Roots by generation, compaction, leases,
budget with low-water mark, hourly collection, identity expiry; the local store's `--stale` collect
and command; the target directory's bound and `blaze clean --keep-latest`. Files:
`crates/blaze-serve/src/collect.rs`, `crates/ra-jit-store/src/lib.rs`, `crates/ra-jit/src/main.rs`
(commands), `harness/dist/gc.py`, `harness/dist/disk.py`. Depends: C5. Acceptance: gc.py's synthetic
publisher (it writes roots and generations through the API, no CI needed): 30 `master` commits each
adding generic instances to the same dependency crate, 10 pull requests opened and closed: within
budget, every one of the last 20 roots builds with 0 functions compiled, obsolete instances' packs
gone, a pinned generation survives a collection that overlaps a running lookup; disk.py: the target
directory bound and the stale identity case of section 7. Verify: `python3 -I harness/dist/gc.py &&
python3 -I harness/dist/disk.py`. Where: Mac.

### Phase 2: trust on the workers, and jobs

**E1 Agents and executors, Linux.** Goal: section 2's roles and isolation rules 1 to 6 on Linux.
`blaze-agent` runs as the control plane's user, keeps trusted builders warm on `master`, creates a
pull request's executor (a fresh user, the whole-job sandbox, the trusted store and image read-only,
an overlay), uploads its outbox with the job's capability, destroys it. Lifecycle as a sans-IO state
machine in `crates/blaze-dist`. Files: `crates/blaze-dist`, `crates/ra-jit/src/worker.rs`,
`crates/ra-jit/src/sandbox.rs`, `crates/blaze-serve` (executor registry). Depends: C6, P0-A, P0-B.
Acceptance, the adversarial test (review B B1): a pull request's job overwrites a known entry in
every store it can reach, plants files in every directory it can write, tries to read the agent's
and builder's credentials and to signal other processes; then a `Main` job runs on the same machine:
its program and its published records are byte-identical to a clean run's, no credential was read,
and nothing of the pull request's executor remains. Verify: `cargo test -p blaze-dist && python3 -I
harness/dist/adversary.py --linux-docker`. Where: Docker; Linux.

**E2 Executors on macOS.** Goal: isolation rules 1 to 6 on macOS: a user per executor and a
Seatbelt profile; a pull request's macOS jobs on the Mac Studio after this. Files:
`crates/ra-jit/src/sandbox.rs` (macOS), `crates/blaze-dist`, `crates/ra-jit/src/worker.rs`.
Depends: E1, P0-E. Acceptance: E1's adversarial test on macOS; a Linux job is never given to a
macOS executor. Verify: `python3 -I harness/dist/adversary.py`. Where: Mac.

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
`never` behave as section 4 says. Verify: `python3 -I harness/dist/jobs.py`. Where: Mac, Docker.

**E4 Admission.** Goal: section 5. Capacities, measured reservations, cgroups on Linux, thread
counts on macOS, `Main` first, the bounded queue, rebuild by drain when overlap does not fit,
stage timestamps on every job and batch; the burst harness for GB. Files: `crates/blaze-dist`
(admission), `crates/blaze-serve/src/jobs.rs`, `harness/dist/burst.py`. Depends: E3. Acceptance:
on one host with a capacity set to two fixture executors, a third job queues with stage
`Queued` and a named wait; a `Main` job overtakes queued pull requests; the queue answers `busy`
past `Q`; a rebuild that does not fit drains first; no reservation ever exceeds capacity in a
1,000-job random schedule (a property test of the state machine). Verify:
`cargo test -p blaze-dist admission && python3 -I harness/dist/burst.py --local`. Where: Mac.

### Phase 3: distributed compile

**D1 The work set.** Goal: section 4's protocol. `compile_unnamed`'s queue behind a trait in
`hir-ty-codegen`, a local and a remote implementation; the coordinator's work set in
`crates/blaze-dist`; leases, batches by home crate, `found`, publish-then-report, refusals,
fencing, the lead's synchronous import and assembly. Files: `crates/hir-ty-codegen/mod.rs`,
`crates/blaze-dist`, `crates/blaze-serve/src/work.rs`, `crates/ra-jit/src/worker.rs`,
`crates/ra-jit-store/src/lib.rs` (import of a pack). Depends: E3, C5, P0-A, P0-B. Acceptance
(`harness/dist/shards.py`, a lead and two helpers on one host, each with the whole front end):
the program equals a one-process build's and its entry digests match; fixtures with mutually
recursive functions across home crates, generics, trait objects, drop glue, statics, a function
blaze refuses (the job fails naming it, nothing goes to rustc), a helper killed while uploading
(its batch is compiled by another, the late result ignored), two leads at once (neither waits on
the other), and cancellation during compile (every executor's process group gone). Verify:
`cargo test -p blaze-dist work && python3 -I harness/dist/shards.py`. Where: Mac.

**D2 Front ends of a closure.** Goal: helpers that load only the closure of the home crates they
serve, from the `BuildSpec`'s unit graph with its features and cfg; the coordinator matches
batches to helpers by closure; small builds lease nothing. Files: `crates/ra-jit/src/main.rs`
(load of a sub-graph), `crates/blaze-dist`, `crates/ra-jit/src/worker.rs`. Depends: D1, C4 (keys
of scripts and macros, so a helper does not rerun every script), P0-C. Acceptance: nushell with two
helpers serving its lowest third of crates: helpers hold less memory than the lead (recorded),
start compiling before the lead is loaded (stage timestamps), and the program equals a one-process
build's. Verify: `python3 -I harness/dist/shards.py --closure ~/work/nushell`. Where: Mac.

**D3 The front-end image.** Goal: #274 for the dependency layer: the front end's results for
dependency crates laid out to be mapped read-only and validated by root, published by trusted
builders under `FrontEndKey` and dependency sources, mapped by executors and helpers. Kill criterion
as roadmap.md section 5's: a part whose mapping and validation costs more than rebuilding it stays
rebuilt. Files: a new crate under `vendor/rust-analyzer/crates/` for the image of front-end state,
`crates/ra-jit/src/main.rs`, `crates/blaze-dist`. Depends: D2. Acceptance: nushell's warm start from
the image against from the store, 3 runs each, same session; two executors on one machine mapping
one image hold less together than two loading it (`RssAnon`, Linux); a pull request's executor that
maps the image cannot change it (E1's adversarial test, image added). Verify: `python3 -I
harness/warmstart.py --image && python3 -I harness/dist/adversary.py --image --linux-docker`. Where:
Mac; Docker.

**D4 Gate GD.** Goal: section 4's gate, measured and acted on. `harness/dist/measure.py` runs
GD's scenarios at 1, 2 and 3 executors, interleaved, n ≥ 3, with spread and limiter; if a bar
fails, the unit names the limiter, applies the next change of section 4's list (or files it as the
next unit), and measures again. Files: `harness/dist/measure.py`, `harness/dist/scenarios.toml`,
`docs/distributed.md` (results). Depends: D2, P0-H; D3 for the scenarios that start executors.
Acceptance: GD's correctness bar on every run; the speed bar met, or the change list's next step
taken and measured. Verify: `python3 -I harness/dist/measure.py --gate GD --runs 3` (Mac and
Docker on fixtures; `--fleet` writes the owner's run form). Where: Mac, Docker, Linux; fleet by
the owner.

### Phase 4: tests

**T1 The cacheability contract.** Goal: section 6's rules 1 to 5. The reach classifier marks time,
randomness, threads, sockets, unlisted spawns, foreign code and unlisted environment as always-run
reasons; an always-run attribute from the owner's manifest; failures never cached; audit mode
(`RA_JIT_TESTS=audit`) and 5% sampling. Files: `crates/hir-ty-codegen/fncache.rs`,
`crates/ra-jit/src/cargo/affected.rs`, `harness/dist/fixtures/tests`. Depends: P0-A. Acceptance:
a fixture of tests that read the clock, use a `HashMap`, sleep, open a loopback socket, spawn an
unlisted tool, read an unlisted variable, and one pure test: only the pure one is skipped on a
second run; audit runs all; a test whose cached pass the audit contradicts loses caching.
Verify: `python3 -I harness/testselect.py && python3 -I harness/dist/tests.py --contract`. Where:
Mac; Docker for the sandbox.

**T2 Passes across machines.** Goal: `TestBaseKey`, `InputManifest` with metadata and absent
paths, logical paths, the server's results index and lookup by namespace, validation on B;
`tests-reach` and `tests-passed` out of the target directory; `CARGO_BIN_EXE_*` by content.
Files: `crates/ra-jit/src/cargo/affected.rs`, `sandboxed.rs`, `crates/ra-jit-store`,
`crates/blaze-serve/src/results.rs`, `harness/dist/crossmachine.py`. Depends: T1, C5, C6.
Acceptance: a test that passed on A is skipped on B, which never ran it; changing a read file's
content or mode on B, creating a path A found absent, or changing an environment variable makes
B run it; a `Pr(n)` pass never answers a `Main` lookup. Verify:
`python3 -I harness/dist/crossmachine.py --tests`. Where: Docker; Linux.

**T3 Bundles and shards.** Goal: `RuntimeBundle`s built by the lead and materialised on helpers;
shards by recorded duration; tracedecay's flake policy (fail) kept. Files: `crates/blaze-dist`,
`crates/blaze-serve/src/jobs.rs`, `crates/ra-jit/src/cargo/affected.rs`. Depends: T2, E3.
Acceptance: a helper runs a test that needs a generated `OUT_DIR` file and a support tool not on
the host's `PATH`; a test needing an undeclared tool fails naming it; a deliberately flaky test
fails the job, as nextest's `flaky-result = "fail"` would. Verify:
`python3 -I harness/dist/tests.py --bundles`. Where: Mac, Docker.

### Phase 5: the switch in tracedecay's CI

**S1 The CI client.** Goal: what a tracedecay workflow calls: join the tailnet, fetch an OIDC token,
`POST /v1/auth`, run `cargo blaze test` with `RA_JIT_DIST` and `LocalPolicy=never`, publish nothing
itself, print the stage timestamps. Files: `harness/dist/ci/blaze-ci.sh`,
`harness/dist/ci/README.md`. Depends: E3, T3, P0-H on both platforms. Acceptance: against the fake
GitHub of C6 and a local server, a pull-request run and a `master` run do what section 2 grants; a
fork run is refused. Verify: `bash harness/dist/ci/blaze-ci.sh --self-test`. Where: Mac.

**S2 The test inventory and comparison.** Goal: review B M10. `harness/dist/ci/inventory.py`
lists every Rust test target, feature variant, custom harness, doctest and generated fixture
tracedecay's Bazel runs, from its BUILD files, and the tests that run rustc or cargo;
`compare.py` fails on a test missing, extra or not executed on either side, and on a different
verdict. Files: `harness/dist/ci/inventory.py`, `harness/dist/ci/compare.py`. Depends: none.
Acceptance: the inventory of `d79de82273` checked in for the owner to approve; compare fails on
a synthetic run that drops a test or adds one. Verify:
`python3 -I harness/dist/ci/compare.py --self-test`. Where: Mac.

**S3 Shadow-mode report.** Goal: G2, G3, GB, GD and GS as the owner measured them, written
here with runs and spread, and a recommendation per owner step. Files: `docs/distributed.md`,
`docs/reports/DIST-SHADOW.md`. Depends: S1, S2, D4, and the owner's runs. Acceptance: each gate's
table filled from the owner's runs, or marked not run. Verify:
`python3 -I harness/dist/ci/compare.py BAZEL_TEST_LOGS BLAZE_EVENTS --inventory INVENTORY`.
Where: fleet by the owner.

### Owner steps, outside the units

These change tracedecay's repository or the fleet, and are the owner's, each after the gate
named:

1. Add `#![recursion_limit = "256"]` to `tracedecay-code-index-runtime` (before P0.8).
2. Grant a Linux host for P0-G (the `linux-host` gate).
3. Deploy `ra-jit serve` and agents on the fleet; set Bazel's reserve `B` (after E4).
4. Add the `rust-blaze` workflow in shadow mode (after S1).
5. Make it blocking (after G2, G3, GB, GD and GT).
6. Remove `linux-test`'s fleet shard and the macOS Rust tests from Bazel; narrow
   `tag:bazel-ci`'s grant to what Bazel still uses (after GS). Rollback: re-enable the removed
   Bazel targets and mark the blaze job non-blocking; nothing on Bazel's side is deleted until
   GS has held for two more weeks.

### Dependencies outside this plan

- **#344, per-item keys** (blaze-next): unchanged functions keep their keys across an edit. Until
  it lands, G2's bar is the affected closure, and signature edits are GD's best case.
- **#147, scripts and macros in the sandbox with read recording**: C4 keys by it on Linux; on macOS,
  E2's sandbox records nothing, so impure scripts and macros run again there.
- **#271, the Merkle VFS**: `SourceRef::Root` replaces commits; an executor takes a job as a
  transaction instead of a checkout.

## Open questions for the owner

- **P0-C's bar.** If the tracedecay worker cannot get under 16 GB on macOS, should verification of
  the full CLI move to a Linux host (the `linux-host` gate), or the guard rise for those runs?
- **Bazel's reserve on Ubuntu-main** (`B`, section 4): how much CPU and memory NativeLink keeps for
  Clippy, release, JS and Python after the switch.
- **The two 12-core machines**: operating system, architecture, and whether the 16 GB laptop is a
  worker at all.
- **Isolation strength**: is a separate user and sandbox per pull-request executor enough for
  same-repository pull requests, or should executors be microVMs (section 2)?
- **Tests that run rustc or cargo**: keep them on Bazel, give them a blaze-provided
  rustc-compatible front end, or change them?
- **Flake policy**: tracedecay's nextest says flakes fail; keep it for the blaze job (this design's
  default)?
- **Rust built for JS or Python tests**: does it keep building through rules_rust, or can Bazel take
  blaze's programs as prebuilt inputs?

## Not checked

- No part of this was built or run for this revision. Every bar is a target.
- Linux: nothing of tracedecay has run on Linux under blaze.
- The fleet's sizes are owner-reported, not measured; the Linux worker's architecture, NativeLink's
  version and configuration, and the hosted runners' sizes were not read.
- Whether blaze's codegen gives byte-identical entries for the same key on two machines (C5).
- Whether Seatbelt still enforces on macOS 26 (E2).
- tracedecay was read at `d79de82273`; its CI may have moved since.

## Appendix A: review findings

`a-` is review A (claims, compliance, phase plan), `b-` review B (design soundness), both of
2026-10-25 on `45449ae6`. **Changed**: the design changed to answer it. **Argued**: kept, with the
evidence given. **Open**: needs the owner. A row can be partly each.

| Finding | What it said | Resolution | Where |
|---|---|---|---|
| a-B1 | rustc only checked after the fact; `RA_JIT_RUSTC_BASE=0` does not stop cargo | **Changed**: one spawn gate refuses before spawn, fails closed in worker mode; an audit with stand-in tools counts attempts; predicate P0-A gates everything after Phase 0 | "The rules this design keeps"; section 0; P0.1, P0.2 |
| a-B2 | reused slots lack PR/Main isolation | **Changed**: trusted builders never run pull-request code; pull-request executors are per pull request, isolated, destroyed; credentials outside every job; an adversarial test | section 2; E1, E2 |
| a-M1 | unknown fleet sizes used as evidence; queue taken as capacity | **Changed**: fleet sizes from the owner, Ubuntu-main conceded as a big box, the queue called a symptom; GB measures concurrent pull requests | "What tracedecay runs today"; sections 5, 9 |
| a-M2 | historical timings lack provenance; hybrid results | **Changed**: a provenance table with machine, runs, spread and hybrid status; the eager-over-lazy conclusion withdrawn | "Numbers kept from revision 1" |
| a-M3 | G0 and key fixes not assigned to units; G2 contradicts renaming | **Changed**: Phase 0 units with tracedecay acceptance; C4 fixes keys before C5; G2's bar is the affected closure | section 0; section 1 table; C4; section 11 |
| a-M4 | wire schema under-specified | **Changed**: field numbers and discriminants for every type, set ordering, hash and signature domains, errors, limits, endpoint payloads; golden vectors in C1 | "Data shapes"; section 8; C1 |
| a-M5 | remote test keys lack input records and baseline; flake exemption undefined | **Changed**: `TestBaseKey` with manifests and validation; the flake exemption withdrawn (tracedecay's fail policy kept); audit and sampling rerun cached passes | section 6; T1, T2 |
| a-M6 | job digest used as execution and cancellation id | **Changed**: `JobContent`, `ExecutionId`, `Attempt`, subscribers; cancel when the last leaves; fencing | "Data shapes"; section 8; E3 |
| a-M7 | codegen shards need dataflow, not one predicate | **Changed**: the batch protocol: work set, leases, publish then report, synchronous import, lead-only assembly, fencing | section 4; D1 |
| a-M8 | gates impossible on the assigned machine | **Changed** and **open**: every gate says where it runs; fleet gates are owner-operated with tooling in units; Linux at scale needs the owner's host | section 11; P0.8; open questions |
| a-M9 | external rollout bundled into units; GC roots without a producer | **Changed**: S1 to S3 land in the fork; owner steps listed apart with rollback; C7's synthetic publisher | Phase 5; "Owner steps"; C7 |
| a-m1 | 763 packages should be 760 | **Changed** | "What tracedecay runs today" |
| a-m2 | verify commands partial; `BIN` ambiguous | **Changed**: `BIN` from the unit's `CARGO_TARGET_DIR`; fixtures pinned; each verify runs all acceptance and fails non-zero | "Phase plan" preamble; every unit |
| b-B1 | PR and Main share a mutable trust domain; the server is a signing oracle | **Changed**: section 2's roles and isolation; `Main` publication only by trusted builders at verified `master` commits; epochs; audit rebuilds. **Argued**: signing authenticates the publisher by design; restricting who may publish is the control, and a compromised trusted builder is the stated residual risk | section 2 |
| b-B2 | a passed `TestKey` is not a safe skip | **Changed**: the cacheability contract; time, randomness, threads, sockets, spawns, foreign code always run; failures never cached; audit | section 6; T1; GT |
| b-M1 | whole-job slots relocate, not distribute; no concurrent-PR gate | **Changed**: distributed compile designed and required (batches over a work set); warm remote jobs renamed and not the deliverable; admission and GB | sections 4, 5; D1 to D4; GB |
| b-M2 | slot memory unsized | **Changed**: P0-B and P0-C bound and size the worker; measured reservations; a per-machine table; drain-then-rebuild | sections 0, 4, 5; P0.3, P0.4, E4 |
| b-M3 | OIDC claims GitHub does not issue; fork policy contradictory | **Changed**: only documented claims, then GitHub API lookups by `run_id` and pull request; capabilities bound to verified source; one fork policy (hosted-only, refused) | section 2; C6 |
| b-M4 | wire request cannot express cargo semantics | **Changed**: `BuildSpec`, `UnitSpec`, `ProfileSpec`, `RunSpec`, refusal before scheduling, differential fixtures | "Data shapes"; C2 |
| b-M5 | cross-machine reuse conditional; deployment identity undecided | **Changed**: C4 fixes the #105 items with a hit-coverage bar (G1); one pinned blaze artifact per platform; `ExecPlatform` | section 1; C4, C5 |
| b-M6 | no read-set index; no runtime bundle | **Changed**: `TestBaseKey` index to manifests; `RuntimeBundle` by digest | section 6; T2, T3 |
| b-M7 | mode D lacks completeness and liveness | **Changed**: coverage per item, terminal batch results, non-blocking compile, bounded assembly waits, fencing, non-blocking helper reservation, process-group cancellation | section 4; D1 |
| b-M8 | zero cargo lines is not the gate; tests run rustc; nightly | **Changed**: execution audit and pre-spawn refusal. **Argued**: tracedecay builds without nightly (its probe falls back, TRACEDECAY.md). **Open**: what to do with tests that run rustc or cargo | sections 0, 6; P0.1, P0.2; open questions |
| b-M9 | crate-only GC roots pin every generic instance | **Changed**: roots by record generation, compaction, leases | section 3 GC; C7 |
| b-M10 | CI switch lacks coverage mapping; flake contract changed | **Changed**: an approved inventory; comparison fails on missing, extra or unexecuted tests; flake policy unchanged | sections 6, 10; S2, S3 |

Counts: 25 findings. Resolved by a design change alone: 22. Changed and argued in part: 2 (b-B1,
b-M8). Changed and open in part, waiting on the owner: 2 (a-M8, b-M8). No finding is left
without a change.

## Appendix B: the research edits

[research/bazel-nativelink.md](research/bazel-nativelink.md) "What this changes in
docs/distributed.md":

| # | Edit | Applied in |
|---|---|---|
| 1 | Trust: NativeLink mixes `master` and pull requests; `Main` only from slots that never ran pull-request code; disposable pull-request state | "What tracedecay runs today"; section 2 |
| 2 | Job identity split; cancel when the last subscriber leaves | "Data shapes"; section 8; E3 |
| 3 | Admission by reservations, `Main` first, stage timestamps, no CPU or memory in keys | section 5; E4 |
| 4 | Tests: Bazel's precedent and limits, always-run, audit, failures never cached, read set with the verdict, runtime bundle, flake exemption the owner's | section 6; T1 to T3 |
| 5 | GC by record generation, completeness, leases | section 3; C7 |
| 6 | Epoch in signed records; forced rebuild of one crate | section 2 "Records, epochs and repair" |
| 7 | `cpu_count` in keys, one instance `main`, client uploads, plaintext gRPC, 5.2 GB inputs | "What tracedecay runs today" |
| 8 | The 200 s pole **unverified**; the run was a release build | section 9 |
| 9 | REAPI: the verdict and reasons of 7.9 | section 3 |
| 10 | A resolved build spec and a native-toolchain digest | "Data shapes"; section 1; C2 |
| 11 | Racing: a first-finishing failure fails; `Never` on hosted runners with tracedecay's reason | section 4 "Failure" |
| 12 | Mode D: no prior system splits one crate; gated | section 4. Changed by the owner's decision: distributed compile is required; the gate decides how, not whether |
