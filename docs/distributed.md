# Distributed cache, compile and tests

2026-10-25. Status: design, nothing built. Written against the fork's `dist/main` at `c8e5f6a8`
and tracedecay at `d79de82273`. Each claim is marked **read** (in a source we read, or measured
by us), **reported** (a document or someone says so, not re-checked here) or **unverified** (our
inference or an estimate). Numbers name the file they come from. Code paths are still `ra-jit`
where the rename to blaze has not reached them.

The goal: blaze gives tracedecay a distributed cache, distributed compile and distributed tests
for Rust on Linux x86-64 and macOS arm64, good enough that tracedecay drops NativeLink for its
Rust build and test actions there. Bazel keeps Windows, JS, Python, Clippy and the release and
dist (thin LTO) builds. Workers run blaze, never rustc ([no-rustc.md](no-rustc.md)).

It builds on [ci-runner.md](ci-runner.md) (#264), which designs the same thing on ix VM forks,
and on [#226](https://github.com/indexable-inc/blaze/issues/226)'s per-crate index. The
difference here: tracedecay has no ix. It has a few persistent machines and GitHub's hosted
runners, so warm state lives in long-running daemons on those machines, not in VM snapshots.

## Short version

- **Identity.** A compiled function is found by `sha256(identity ‖ entry key)`, as today
  (`Identity` in `crates/ra-jit-store/src/lib.rs`, **read**). Nothing new is hashed into it. A
  job, a shard and a test verdict get content keys of the same shape. The target triple is in
  every entry key, so Linux and Mac entries never meet. Cross-machine hits need the #105 list
  closed for what each phase shares; the first unit is a harness that lists every miss.
- **Cache.** `ra-jit serve`, one process on Ubuntu-main over the existing pack layout, adds
  what S3 lacks: one batch lookup of per-crate records, authorisation by GitHub OIDC claims,
  namespaces (`main`, `pr/<n>`), server-signed records and GC by live roots. The S3, directory
  and https places stay as they are; MinIO is not the fleet's server.
- **Compile.** The unit of distribution is the **job on a warm slot**: a daemon on a fleet
  machine, warm on `master`, takes the job's tree as an edit, builds and tests it. A crate is
  not a unit, because blaze has no per-crate action; a function is not, because it costs
  milliseconds. Within a large job, **codegen shards** split the instances across several warm
  slots, behind a gate that can cut them.
- **Tests.** Test binaries are cut into shards by recorded duration and run in the sandbox on
  slots of the right platform. A pass is kept under a content key and skipped on every branch
  with the same code and inputs. Two verdicts under one key are a flake.
- **Wire.** One versioned schema in deterministic CBOR (RFC 8949 section 4.2) over HTTP inside
  the tailnet; blobs are raw bodies by digest. Hosted runners join the tailnet as ephemeral
  tagged nodes, as they do for NativeLink today. No gRPC, no MCP; REAPI only as the edge adapter
  #226 describes, not planned here.
- **Objections.** Per-function `Execute` loses: agreed, and nothing here sends a function. ix's
  "width beyond one box buys about 5%" is conceded for one job on one large box. tracedecay has
  no large box, half of its uncached critical-path Rust time was queueing, and warm daemons need
  persistent machines: the fleet buys capacity and warmness first, width second, and width is
  gated.
- **Plan.** Eighteen units: two measurement harnesses first, then the cache, then the keys it
  needs, then jobs on warm slots, tests, the Mac, codegen shards, and the switch in tracedecay's
  CI. Every speedup claim waits on a gate measured against a baseline in the same session.

## What tracedecay runs today

All **read** from tracedecay at `d79de82273` unless marked.

- **The fleet.** A NativeLink coordinator on Ubuntu-main; workers Ubuntu-main, Omarchy and one
  Mac. The Mac serves macOS actions only; Linux actions never go to it
  (`docs/ci-remote-builds.md`). Core counts and memory of the three are not in the repository
  (**unverified**).
- **Hosted runners join over Tailscale.** The official Tailscale action enrols the runner as an
  ephemeral node by OIDC, tagged `tag:bazel-ci`, which may reach only Ubuntu-main TCP 50051
  (cache and execution), 1985 (build events) and 443 (reports). The OIDC identity is bound to
  `repository_id` 1261602522, `actor_id` 25274700 and the `ci.yml` workflow ref. Fork pull
  requests are excluded and run hosted-only (`docs/ci-remote-builds.md`, `ci.yml` line 345).
- **What runs remotely.** `--strategy=Rustc=remote` with `--noremote_local_fallback` ("Fleet
  compilation must not fall back onto the smaller hosted VM"), `--strategy=TestRunner=dynamic`,
  `--jobs=40`, instance `main` (`.github/actions/setup-bazel/action.yml`, lines 121 to 139).
  `docs/ci-remote-builds.md` says Rustc uses dynamic execution; the action file says remote only.
  This design takes the action file as what runs.
- **Test shards.** A trusted run is one `fleet` shard; a hosted-only run is six (`root-a`,
  `root-b`, `workspace-a`, `workspace-code`, `workspace-b`, `workspace-c`), timeout 240 minutes
  (`ci.yml`, `linux-test`).
- **Linux workers are hermetic** through a zig `cc` toolchain (`.bazelrc`, `rbe-linux`). The Mac
  has a newer Xcode and SDK than the hosted `macos-26` image, so matching OS and CPU does not
  make native compiles compatible (`docs/ci-remote-builds.md`).
- **Size.** 51 crates under `crates/`, 763 packages in `Cargo.lock`.
- **What one measurement found** (`.codex/pstack-runs/pr-merge-sweep/ci-scheduling-measurement-analysis.md`;
  one interrupted diagnostic run of a release CLI build, run 39830079562, Bazel 9.3.1, not a
  benchmark, no repeats): five uncached critical-path Rust actions spent 479.395 s queued and
  352.217 s executing, so queueing was 51.8% of their 924.604 s. The `tracedecay` crate was the
  exception: 10.504 s queued, 200.846 s executing, one rustc action. A git submodule update for
  `tokensave-large-treesitters` took 487.687 s during analysis. The analysis names no limiter
  for the queue: the fleet's dispatch records were not available.

## What blaze has today

All **read** at `c8e5f6a8`.

- **The store** (`crates/ra-jit-store/src/lib.rs`): immutable packs named by their SHA-256,
  values in zstd blocks behind an index, one open pack per daemon, lock-free reads and adds,
  LRU collection by pack (`StoreDir::collect`). A key on disk is `sha256(identity ‖ key)`, where
  `Identity::new` hashes the store format, what blaze was built from (`built_from`, a hash
  `crates/ra-jit/build.rs` takes of the sources, features, `TARGET` and debug assertions) and
  the toolchain.
- **The entry key** ([cache.md](cache.md)): backend and optimisation level, target triple,
  debug info, overflow checks and the function's stable name (`stable.rs`). A dependency's
  crate is named by its stable crate id; a crate of the checkout by a hash of its source
  (`content.rs`, `ContentId` in `hir-ty-codegen/mod.rs`). An edit renames the edited crate and
  every checkout crate that depends on it.
- **The remote** (`crates/ra-jit/src/remote.rs`, `remote/`): `Place::Dir`, `Place::S3`,
  `Place::Https`; `packs/`, `keys/`, `manifests/<project>`; verified transfers with resume
  (`transfer.rs`); Ed25519-signed manifests with turns (`signed.rs`); remote GC by the clock
  (`collect.rs`); a daemon's push and pull in the background (`keeper.rs`). Measured on
  loopback against MinIO: a fresh machine building cargo 0.92.0 took 152.4 s with no remote,
  117.1 s with blaze's packs, 94.4 s with packs and cargo's units, compiling 0 functions in both
  remote rows ([reports/REMOTE.md](reports/REMOTE.md), one run each).
- **What a build schedules.** One daemon compiles "a set of root functions into one object"
  (`CodegenUnit::compile`), each function a salsa query whose result names its callees by
  stable name (`fragment.rs`), on every core (`SymbolResolver::in_parallel`). There is no
  per-crate action: one database, all crates, all cores. `RA_JIT_UNITS=1` groups functions into
  LLVM modules for whole-program builds (`units.rs`); the daemon prints phase times with
  `RA_JIT_STATS` (`main.rs`, "phases: compile, resolve, define, fallback, object").
- **What a whole build costs** ([no-rustc.md](no-rustc.md), "The cold start without rustc",
  dev-aws-8, 96 cores pinned, every crate blaze's):

  | | cargo cold | blaze, empty store | warm store | empty store, warm remote |
  |---|---|---|---|---|
  | ripgrep | 5.53 to 5.76 s | 16.8 to 17.4 s | 3.98 to 3.99 s | 4.01 to 4.02 s |
  | nushell | 33.1 to 33.2 s, 2.4 GB | 78.9 s, 23 GB | 31.3 s, 13.8 GB | 31.5 s |
  | helix | 31.3 to 31.4 s, 1.8 GB | 87.0 to 89.7 s, 23 GB | 25.5 to 25.7 s, 13 GB | 26.0 to 26.3 s |

  Of nushell's 31 s with everything in the store: resolving 3.8 s, asking the store about roots
  3 s, putting functions in the object 4 s, describing them 4 s, writing the object 7 s, listing
  4.7 s, the link 1 s. So with a perfect cache nushell's floor is about 31 s, not 0, until those
  get faster.
- **One daemon stops scaling.** Compiling the standard library is 4.5 s on 16 cores and 3.9 s on
  96: a fifth of the samples are in rust-analyzer's interning and the lock slow paths beside it
  ([no-rustc.md](no-rustc.md), "The standard library, shipped").
- **A warm daemon.** nushell: a body edit 258 ms, a signature edit 268 to 285 ms, ready from
  nothing in 52.1 s, 5.2 GB resident when ready, 7.5 GB after its own base, 11.2 GB at peak
  ([roadmap.md](roadmap.md), [ci-runner.md](ci-runner.md)). A restart onto its own store went
  from 30.8 s to 21.4 s with #295 (#105, dev-aws-5, **reported** there).
- **Tests.** `RA_JIT_TESTS=affected` runs only tests whose reach hash changed since they passed
  (`cargo/affected.rs`); a test that reaches outside its code runs in `ra-jit sandbox`, which
  records every path it touched (`cargo/sandboxed.rs`, `sandbox.rs`). The sandbox is Linux
  only; elsewhere such a test runs every time. `tests-reach` and `tests-passed` live beside the
  test program in the target directory. The per-test seed of #257 (`RA_JIT_TEST_SEED`) is not
  in the tree.
- **The socket** speaks one JSON line a request (`socket.rs`, `LONGEST_REQUEST` 16 MiB). No CBOR
  crate is in `Cargo.lock`; `hyper`, `rustls` and `ed25519-dalek` are.

## Data shapes

These are the types the units below implement. They are written as Rust; on the wire each
struct is a CBOR map keyed by the small integers in the comments, in ascending order, as
[roadmap.md](roadmap.md#8-the-agent-api) requires. `Digest` is a SHA-256.

```rust
type Digest = [u8; 32];

/// What a store's keys are hashed with: `Identity` in ra-jit-store, unchanged.
/// sha256(store FORMAT, built_from sources hash, toolchain commit).
struct StoreIdentity(Digest);

/// The caller's key of one compiled function, unchanged: sha256(backend, opt level,
/// target triple, debug info, overflow checks, stable name). On disk and on the wire
/// a function is only ever named by sha256(identity ‖ EntryKey).
struct EntryKey(Digest);

/// The home crate of a set of entries: the crate a stable name says an instance belongs to
/// (`home.rs`). sha256("crate\0" ‖ identity ‖ target ‖ profile key ‖ crate's stable id),
/// where the stable id is a ContentId for a checkout crate and the stable crate id for a
/// dependency. The per-crate record is filed under it.
struct CrateKey(Digest);

/// The settings an entry key already hashes, named once for a build:
/// backend, opt level, debug info, overflow checks, cfg(test).
struct ProfileKey(Digest);

struct PackRef {           // 0..=4
    pack: Digest,          // sha256 of the pack as the store has it
    size: u64,
    keys: Digest,          // sha256 of its key list (keys/<digest>)
    packed: Option<(Digest, u64)>, // the .zst object, when sent compressed
    format: u32,           // store FORMAT
}

/// The one mutable object of the cache: which packs hold a crate's entries in a namespace.
/// It is a manifest of today's format scoped to one crate, signed by the server.
struct CrateRecord {       // 0..=4
    namespace: Namespace,
    crate_key: CrateKey,
    packs: Vec<PackRef>,
    turn: u64,             // as signed.rs: only goes up
    signature: [u8; 64],   // Ed25519 by the server, over namespace, crate_key, turn, packs
}

enum Namespace {
    Main,                  // written only by builds of master's lineage
    Pr(u64),               // written by enrolled jobs of that pull request, read by them only
    Std,                   // the shipped standard library (shipped.rs), read-only
}

/// Which tree a job builds. Until the Merkle VFS (#271) lands, a git commit the workers
/// can fetch; then its root.
enum SourceRef {
    Git { repo: String, commit: [u8; 20], tree: [u8; 20] },
    Root { root: Digest },  // BLAKE3 Merkle root, roadmap.md section 1
}

/// What a warm daemon is keyed by (architecture.md "Processes"), plus the lock file,
/// which decides the dependency crates' ids.
struct WorkspaceKey {      // 0..=6
    repo: String,
    identity: StoreIdentity,
    host_triple: String,   // the blaze binary's, which is the target: no cross builds
    features: Vec<String>,
    cfg_test: bool,
    lock: Digest,          // sha256 of Cargo.lock
    profile: ProfileKey,
}

struct JobRequest {        // 0..=7
    workspace: WorkspaceKey,
    source: SourceRef,
    want: Want,
    crates: Vec<(String, Digest)>, // the requester's crate names and ContentIds, when it loaded
    namespace: Namespace,  // where the job's packs and verdicts go
    deadline_ns: u64,
    local: LocalPolicy,
}

enum Want { Check, Build { programs: Vec<String> }, Test { filter: Vec<String> } }
enum LocalPolicy { Never, Fallback, Race }

/// One slice of a job's codegen (decision 3, mode D).
struct ShardSpec {         // 0..=4
    job: Digest,
    source: SourceRef,
    index: u32,
    count: u32,            // instances whose stable-name hash mod count == index
}

enum JobEvent {
    Assigned { worker: String, slot: u32 },
    Stage { name: String, elapsed_ns: u64 },
    Published { records: Vec<CrateKey> },
    Program { name: String, blob: Digest },   // an executable, for shards and the requester
    Verdict(TestVerdict),
    Done { outcome: Outcome, stats: Vec<(String, u64)> },
}

/// A test's content key (ci-runner.md "What a test result is keyed by").
/// sha256(identity, target, reach hash, attributes, fixed environment, the content of each
/// CARGO_BIN_EXE_* program, the digests of every path the sandbox recorded).
struct TestKey(Digest);

struct TestVerdict {       // 0..=5
    key: TestKey,
    binary: Digest,
    name: String,
    passed: bool,
    duration_ns: u64,
    output: Option<Vec<u8>>, // failures only, bounded
}

/// What one build at one commit used: the GC's live roots.
struct RootManifest {      // 0..=3
    namespace: Namespace,
    source: SourceRef,
    crates: Vec<CrateKey>,
    tests: Vec<TestKey>,
}

struct SlotState {         // 0..=4
    workspace: WorkspaceKey,
    at: SourceRef,         // the tree its daemon has loaded
    resident: u64,
    busy: Option<Digest>,  // the job it runs
    ready: bool,
}
```

What the server keeps, on its own disk:

| Table | Key | Value | Mutable |
|---|---|---|---|
| `blobs/` | SHA-256 of the bytes | packs, key lists, executables, as the `Dir` place lays them out | no |
| `records/<ns>/` | `CrateKey` | the latest signed `CrateRecord` | yes, by conditional write and a higher turn |
| `results/<ns>/` | `TestKey` | `TestVerdict` of a pass | append, then immutable |
| `roots/<ns>/` | `SourceRef` | `RootManifest` | yes |
| `jobs` | job digest | request, assignment, events | in memory, with a log |
| `workers` | worker name | `Vec<SlotState>`, last heartbeat | in memory |

## 1. Identity

**Chosen.** Keep `Identity` and the entry key as they are. Add three keys of the same shape:
`CrateKey` (the record a lookup asks for), the job digest (`sha256` of the encoded
`JobRequest` without its deadline and policy) and `TestKey`. The source tree is named by a git
commit until #271's Merkle root exists, and never enters a function key: function keys are
already content keys, so two jobs on different commits share every crate they agree on. The
lock file enters only `WorkspaceKey`, to pick a warm slot whose dependency crates are the
job's.

Facts that decide it:

- `built_from` hashes blaze's sources with its `TARGET` (`build.rs` line 53, **read**), so the
  Linux and Mac binaries have different identities and share nothing, which is right: they
  compile for different targets anyway. Every fleet machine and runner of one platform must run
  a blaze built from the same sources and toolchain; the server refuses a `Hello` whose identity
  is not one it serves.
- The entry key has the target triple (**read**, cache.md), so a namespace holds both
  platforms without collision.
- A checkout crate's id is a hash of its source with `$PACKAGE`, `$OUT` and `$CHECKOUT`
  placeholders (#105, **reported** there). Dependencies' ids are stable crate ids; a crate id
  from an absolute registry path outside `CARGO_HOME` is still path-dependent (#105 item 4,
  **reported**).

**What must become content-only first, and for what.** From #105's ranked list (**reported**),
with the phase that needs each:

| #105 item | Path-dependent today | Needed for |
|---|---|---|
| crate ids from absolute registry paths outside `CARGO_HOME` (`own_crate_id`) | yes | function hits across machines (units 2 and 5); vendored or path dependencies only |
| build-script run keys (`cargo/script.rs` `run_key`) | hash the script's executable, which differs between target directories | a cold requester not running every build script (unit 9); not function hits, as long as scripts write the same files |
| proc-macro expansion keys (`ra-jit-expand`) | hold the cwd and environment | a cold requester not expanding every macro (unit 10) |
| per-target-dir records (`tests-reach`, `tests-passed`, `.ra-jit-units`, integration-test binaries by size and mtime) | yes | test verdicts across machines (unit 11) |
| the identity hashes what blaze was built from | content-only: `built_from`, fed by `crates/ra-jit/build.rs` (**read**; #326 on #105) | nothing further |

A function hit across machines also needs the code itself to be path-free: a panic location or
`file!()` of a registry crate names `$CARGO_HOME/registry/src/...` (`blaze-registry` makes those
paths cargo's, **read** in its module docs). Whether a stored function carries machine A's path
into machine B's program was not checked (**unverified**). Unit 2 checks it, with panics,
`file!()` and debug info in its fixture.

**Rejected.** A new hash over the whole source tree as the cache key (Bazel's input root): an
edit anywhere would miss everything. Hashing the lock file into function keys: dependency crate
ids already carry name, version and metadata. Cross-host identities that let a Linux blaze
compile for macOS: the binary's target is in its identity on purpose, and whether codegen for a
target is the same on every host (const evaluation with host floats, for one) is not known
(**unverified**); it is an open question, not a phase.

## 2. Distributed cache

### The server

**Chosen: `ra-jit serve --cache DIR`**, on Ubuntu-main, over the `Dir` place's layout, with the
S3, directory and https places kept as they are for anyone else. One process also hosts the
coordinator (decision 3). What it adds over a bucket:

- **Batch lookup.** `POST /v1/cache/lookup` takes the `CrateKey`s of a build (506 crates on
  nushell, #226 comment, **reported**) and answers with their records in one response. A bucket
  needs a GET per record: 506 GETs at 8 in flight (`RA_JIT_REMOTE_JOBS`) and a 20 ms tailnet
  round trip is about 1.3 s (**unverified** arithmetic; the round trip is a guess).
- **Authorisation by who the job is**, not by bucket credentials on a runner (below).
- **Namespaces** and server-side signing, so a pull request's packs never reach `master`'s
  builds.
- **GC by live roots**, which a bucket has no way to run on its own (REMOTE.md: "Nothing runs a
  remote collection on its own", **read**).

**Rejected: MinIO behind the S3 place as the fleet's server.** It works today (REMOTE.md's harness ran
against it and SeaweedFS, **read**), and stays supported. It loses on each point above: one GET
per record, S3 credentials on every hosted runner, policy in bucket rules that cannot see a
GitHub claim, and a collector someone has to schedule. It is also a second service beside the
coordinator, which needs an HTTP server anyway. **Rejected: both as equals.** One server for the
fleet; the places stay for everyone else.

### Lookup granularity

**Chosen: per crate.** A push cuts packs by home crate and publishes one `CrateRecord` per
`CrateKey`; a build looks up the records of the crates it loaded, in one batch, then fetches
only the packs of those records that it lacks (eagerly, before the build, as REMOTE.md found
eager faster than lazy on a fresh machine: 94.4 s against 100.2 s for cargo, **read**).

Why not the others:

- **Per function** (REAPI's `GetActionResult`): 165,752 lookups on nushell, 18.6 s serially at
  gRPC's 112 us unary median (ci-runner.md and #226, **read**, the median **reported**).
- **Per project manifest** (today): a manifest lists every pack any build pushed. A pull fetches
  them all; lazy fetching fetches by key but "does not win on a fresh machine", and "packs cut
  by crate so that a build wants fewer of them, were not tried" (REMOTE.md, **read**). Per crate
  is that untried cut.

A crate with many generic instances (a dependency such as `alloc` in every program's types)
collects packs from many builds. Its record lists them all; the key lists say which hold what
a build wants. Merging a crate's small packs is the store's existing `merge_small`, run by the
server per crate.

### Auth and trust

- **Network.** The server listens on one TCP port inside the tailnet. A new tag,
  `tag:blaze-ci`, may reach only that port on Ubuntu-main, as `tag:bazel-ci` reaches 50051
  today. Fleet machines are fixed nodes.
- **Who is asking.** A hosted job sends a GitHub OIDC token (the `linux-test` job already has
  `id-token: write`, **read**) with audience `blaze-serve`. The server verifies it against
  GitHub's keys and maps claims to grants with a policy file:

  | Claims | Grant |
  |---|---|
  | `repository_id` 1261602522, enrolled `actor_id`, `ref` = `refs/heads/master`, `event_name` = `push` | read `Main`; write `Main` |
  | same repository and actor, `event_name` = `pull_request`, head repository = this one | read `Main` and `Pr(n)`; write `Pr(n)` |
  | anything else that reaches the port | read `Main` |

  A fleet worker holds a token issued at enrolment (a file, mode 0600) granting what the job it
  runs is granted, never more. A developer's machine gets a read token.
- **What is trusted.** Packs are checked against their digest on every fetch, as today. Records
  are signed by the server's Ed25519 key over namespace, crate key, turn and packs; clients pin
  that key in `RA_JIT_REMOTE_TRUST`, so `signed.rs` and `Turns` apply unchanged. The signature
  covers the namespace, so a `Pr` record cannot be replayed as `Main`.
- **Why namespaces.** A job runs the pull request's build scripts, proc macros and tests before
  it publishes. Code that escapes a sandbox can write a well-formed pack mapping a real key to
  other code (ci-runner.md, "Where new compiled functions go", **read**). In `Pr(n)` that code is
  read only by that pull request. `Main` is written only by builds of `master`. A pull request's
  new entries are compiled again by `master`'s build after the merge; with a warm slot that is
  an edit (decision 3). Fork pull requests get no token and no tailnet, as today.

**Rejected.** Signing keys on hosted runners (`RA_JIT_REMOTE_SIGN` as a GitHub secret): any
workflow that can read the secret can sign for `Main`. Unsigned records with network trust
only: whoever can reach the port says what every build runs. Promoting a pull request's packs
into `Main` on merge: it would make `Main` trust code built beside untrusted tests; compiling
again is the price.

### GC

- `master`'s builds publish a `RootManifest` per commit. Live roots: the last 20 commits of
  `master`, and any root written in the last 14 days. A `Main` record named by no live root is
  kept by least recent use within a byte budget (`--max`).
- `Pr(n)` is dropped by `DELETE /v1/ns/pr/<n>` from a `pull_request: closed` workflow, or 14
  days after its last write.
- A blob no record names is removed after the existing grace (at least two hours,
  `collect.rs`, **read**), checked again right before each removal, so a publish in flight is
  safe by the same clock argument as today.
- The local store keeps its LRU under `RA_JIT_CACHE_MAX`, unchanged.

## 3. Distributed compile

### The unit of distribution

| Unit | Verdict | Why |
|---|---|---|
| one function per `Execute` | rejected | about 2 ms of thread time a function on nushell (21.2 s on 16 cores for 165,752, roadmap.md, **unverified** arithmetic), against a request and an input tree per call |
| one crate per action (Bazel's unit) | rejected | blaze has no per-crate action: a crate's functions are queries over the whole database, so a worker for one crate loads its whole dependency closure, and a big crate is still one long pole |
| **one job on one warm slot** | **chosen, mode C** | a warm daemon makes a pull request an edit (258 ms for a nushell body edit, **read**); a crate is spread over every core of the slot, so the `tracedecay` crate is not one 200 s action |
| **codegen shards of one job over several warm slots** | **chosen, mode D, gated** | splits the instances of a large job; each slot pays the front end of the edit, which it has memoised from `master` |
| one big box | conceded where it exists | tracedecay has none; see decision 6 |

A **slot** is one worktree on a fleet machine with one blaze daemon in it, keyed by
`WorkspaceKey`, warm on `master`. A machine has as many slots as its memory holds: nushell's
daemon is 7.5 to 11.2 GB (**read**); tracedecay's is gate G0's number.

**Mode C.** The coordinator gives the job to the free slot whose loaded tree is cheapest to move
to the job's tree: the checkout crates the diff touches and their dependents, a lock file
change counting as a reload (ci-runner.md step 2's rule). The slot fetches the commit, checks
it out in place, and builds; the daemon finds the changed files at the build request (`tree.rs`)
or is handed them from `git diff --name-status`. It publishes its packs to the job's namespace,
runs the tests (decision 4), streams events, and stays at that tree for the branch's next push
or moves back to `master` when idle.

**Mode D.** For a job whose instance count passes a threshold (gate G4 sets it), the lead slot
asks for helper slots warm on a tree near the job's. Each moves to the job's tree, walks the
reachable instances as a build does, compiles only those whose stable-name hash modulo the shard
count is its index, and publishes them. The lead compiles its own share, then builds the object
from the store as the warm-remote path does today. A function's result names its callees by
stable name (`fragment.rs`, **read**), so a function compiles without its callees being
compiled; what each helper repeats is the walk and the MIR of what it does not compile. Where a
store entry for an instance exists, its relocations already list its callees, so the walk reads
them instead of building MIR (**unverified** that this is cheaper; unit 17 measures). Whether
mode D pays is the share of codegen in a large edit: unit 17's gate.

### How a worker gets front-end state

| Option | Cost, measured | Verdict |
|---|---|---|
| **warm daemon on `master`, job as an edit** | body edit 258 ms, signature edit 268 to 285 ms on nushell; ready from nothing 52.1 s once per blaze binary (**read**); 7.5 to 11.2 GB a slot (**read**) | **chosen** |
| each worker loads the workspace cold per job | nushell restart onto its own store 21.4 s (#105, **reported**); a whole build with every function from the store 31.5 s, of it resolving 3.8 s and listing 4.7 s (**read**); ripgrep loading 0.46 s and resolving 0.38 s (**read**) | fallback for a slot that is not warm yet; fine for small workspaces, two orders of magnitude over an edit on nushell's size |
| ship MIR to workers | no serialised MIR exists; MIR names interned ids of one database, and codegen asks the database for layouts, vtables, type ids and symbols. The only serialised front-end state is frozen def maps, impls and lang items (#40), not bodies (**read**, no-rustc.md) | rejected: a second representation of most of the database, with no number to justify it |

A slot is rebuilt when the identity changes (a new blaze binary or toolchain), when the
`WorkspaceKey` changes, when its daemon's memory passes a budget (workers grow with edits,
#261, **reported** in ci-runner.md), and daily. A rebuild starts beside the old slot and swaps.

### Results

A slot publishes packs to the job's namespace and returns records and, when asked, executable
blobs. **Whoever runs a program links it**: a developer's `cargo blaze run` links on the
developer's machine from the store; a CI job's test programs are linked by the slot that runs
them. The coordinator never links. The splice and live edits stay on the machine of the
running program; they are never remote.

A cold requester (a hosted runner, a developer's laptop on a new branch) gets hits for crates
the change did not reach. It misses on everything downstream of the edit, because an edit
renames its crate and the checkout crates that depend on it (cache.md, **read**). Per-item keys
(#344, blaze-next, **reported** on #105) would keep unchanged functions' keys across an edit;
until they land, a pull request's downstream crates are compiled by its job, and mode C is what
makes that cheap.

### Scheduling across platforms

- A slot advertises its `SlotState`; a job needs a slot whose `host_triple` is its target, whose
  identity is the requester's, and whose `lock` matches unless the job accepts a reload.
- Linux x86-64 jobs go to Ubuntu-main and Omarchy; macOS arm64 jobs go to the Mac, as
  NativeLink is configured now. No cross-compiling.
- Among eligible slots: least diff cost, then least loaded machine. A job waits for a slot no
  longer than its `deadline_ns`, then follows its `LocalPolicy`.

### Failure

- **A slot dies or stops sending heartbeats** (every 5 s; three missed): its job or shard goes to
  another slot once; a second loss fails the job with the reason.
- **Local fallback and racing**, per requester: `Never` (hosted runners, mirroring
  `--noremote_local_fallback`: the hosted VM is the smaller machine, **read**), `Fallback`
  (build locally after the deadline or a remote failure), `Race` (build locally from the start,
  as Bazel's dynamic execution does; the first finished result wins and the other is
  cancelled). Racing is per job, not per function.
- **Cancellation**: `DELETE /v1/jobs/<id>`, or the requester's connection closing. The slot stops
  at the codegen's existing interruption point (the `done` callback of `compile_unnamed`,
  **read**) and keeps what it compiled.
- **A job never goes green through cargo or rustc.** A slot that prints `building with cargo`
  fails the job as a blaze bug, as ci-runner.md's runner rule says. A test failure is a result,
  not an infrastructure failure, and is never retried elsewhere except under the flake rule.

## 4. Distributed tests

- **The unit is a shard of one test binary**: the binary's digest and a list of test names
  (`--exact`). Shards are cut by each test's last recorded duration, longest first onto the
  least loaded slot; a test with no record counts as the median. A binary with a few long tests
  is cut per test; one of many short tests in a few shards.
- **Where.** On the slot that built the binary first. A helper slot of the same platform gets the
  binary as a blob from the server, and the source tree at the job's commit from its own clone,
  for the fixtures tests read.
- **Sandboxing.** Each test that reaches outside its code runs in `ra-jit sandbox` (#257), as
  `cargo/sandboxed.rs` does locally; about 16 ms a process on Linux (ci-runner.md, **reported**).
  On macOS there is no sandbox (`sandbox.rs`, **read**): a pure test is keyed by its reach hash
  alone, and an impure one runs every time, as locally.
- **Results by content.** A pass is kept under its `TestKey` in `results/<ns>`. A job skips a test
  whose key passed in `Main` or in its own namespace, and says so ("skipped: passed at abc1234
  under the same key"). A failure is never a reason to skip. This is ci-runner.md's slice R2 on a
  server instead of an ix seed: `tests-reach` and `tests-passed` move out of the target directory,
  and `CARGO_BIN_EXE_*` programs are named by content, not size and mtime (`affected.rs`).
- **Flakes.** Two verdicts under one key are nondeterminism by construction. A failure under a key
  with a recorded pass reruns three times in fresh sandboxes on the same slot; any pass marks it
  flaky, with "passed 2 of 3 under this key" in the report. A flaky test whose key the pull
  request did not change does not fail it; one whose key it changed does. The per-test seed
  (`RA_JIT_TEST_SEED`, a hash of the key) is #257's and not in the tree; until it is, randomness
  is a source of flakes the key does not see.

## 5. Wire protocol

- **Encoding.** Deterministic CBOR, RFC 8949 section 4.2, with integer map keys and no floats, a
  validator that refuses any message that is not in that form, and JSON only as a rendering
  (`Accept: application/json`, `--json`), exactly as [roadmap.md](roadmap.md#8-the-agent-api)
  decides for the agent API. One crate, `crates/blaze-wire`, holds the schema for both. The
  roadmap names minicbor; it is not in `Cargo.lock` yet (**read**).
- **Transport.** HTTP/1.1 on the `hyper` and `rustls` already in the tree, `application/cbor`.
  Inside the tailnet WireGuard encrypts; TLS is used when the server is reached outside it. A
  stream of events is a chunked body of items, each a four-byte big-endian length and one CBOR
  item, the socket framing roadmap.md gives.
- **Blobs** are not CBOR: `GET /v1/blob/<sha256>` with `Range` (what `transfer.rs` resumes with),
  `PUT /v1/blob/<sha256>`, checked by the server before it answers.
- **Versioning.** `POST /v1/hello` carries the versions a client speaks; the server answers with
  the one it picked, its signing key, its limits and the identities it serves. Within a version a
  decoder refuses unknown keys, so two sides never disagree on what a message means. A change
  that adds a field is a new version; the server keeps the version before for one blaze release.
- **Messages:**

  | Call | Request | Answer |
  |---|---|---|
  | `POST /v1/hello` | versions, identity, host triple, role | version, server key, limits |
  | `POST /v1/cache/lookup` | namespaces to read in order, `Vec<CrateKey>` | `Vec<Option<CrateRecord>>` |
  | `POST /v1/cache/missing` | `Vec<Digest>` | the ones the server lacks |
  | `GET`, `PUT /v1/blob/<sha256>` | bytes | bytes |
  | `POST /v1/cache/publish` | namespace, `CrateKey`, `Vec<PackRef>`, the turn it read | the signed `CrateRecord`, or "turn moved" |
  | `POST /v1/results/lookup` | `Vec<TestKey>` | `Vec<Option<TestVerdict>>` |
  | `POST /v1/results/publish` | namespace, `Vec<TestVerdict>` | count kept |
  | `POST /v1/roots` | `RootManifest` | ok |
  | `DELETE /v1/ns/pr/<n>` | | ok |
  | `POST /v1/jobs` | `JobRequest` | job digest |
  | `GET /v1/jobs/<id>/events` | | stream of `JobEvent` |
  | `DELETE /v1/jobs/<id>` | | ok |
  | `POST /v1/workers/hello` | name, cores, memory, `Vec<SlotState>` | worker token check |
  | `POST /v1/workers/next` (long poll) | free slots | a job or a `ShardSpec` |
  | `POST /v1/workers/events` | job digest, `Vec<JobEvent>`, heartbeat | cancel or carry on |

- **Joining from a hosted runner.** The Tailscale action with an OIDC client whose only scope is
  auth keys for `tag:blaze-ci`, bound to the same claims as today's `TS_BAZEL_CLIENT_ID`; an
  ephemeral node logged out at the job's end. Then a GitHub OIDC token for the server. Two
  independent checks, as tracedecay's doc describes for Tailscale and the workflow condition.
- **Rejected.** gRPC on the hot path (REAPI's unary AC lookups, above; and a second stack beside
  the agent API's). JSON on the wire (the agent API decided CBOR; one schema serves both).
  The daemon's JSON-line socket protocol extended over TCP: it is per user and local by design
  (`socket.rs`).

## 6. Upstream's objections

- **"Per-function `Execute` and per-function ActionCache lookups lose"** (#226, ci-runner.md).
  Agreed, with their numbers: 165,752 lookups, 18.6 s serially. Nothing here sends or looks up
  one function. The units are a job, a codegen shard (thousands of instances), a test shard, and
  a crate's record.
- **"Width beyond one box buys about 5%"** (ix's measurement on a 192-thread box, ci-runner.md,
  **reported**). Conceded for one job on one large box. What differs for tracedecay:
  - There is no large box: three machines of sizes not stated, and hosted runners
    (**unverified** sizes). The question is not width over one box but where warm daemons can
    live, and hosted runners cannot keep one.
  - Half of the measured uncached critical-path Rust time was queueing (51.8%, one run,
    **read**): the fleet's problem was capacity and dispatch, not width. A warm slot turns a
    pull request into an edit, which uses less of the fleet per job (**unverified** until G3).
  - The longest action, the `tracedecay` crate at 200.846 s, is one rustc action on one worker.
    blaze spreads one crate over every core of a slot (**read**, `CodegenUnit::compile`), so that
    pole does not exist in the same shape.
  - One blaze daemon's library compile stops scaling at 16 cores (4.5 s on 16, 3.9 s on 96,
    **read**). Mode D works across slots on one machine as well as across machines, so if width
    pays at all it may pay on one machine first.
  - **Open:** whether mode D beats one slot for tracedecay. Gate G4 decides; if it fails, mode D
    is not shipped and the design is mode C plus the cache.

## 7. What stays on Bazel

| Kept on Bazel | Why |
|---|---|
| Windows | blaze builds nothing on Windows yet: "On Windows every build is cargo's" ([usage.md](usage.md), **read**) |
| JS, Python | Rust only, by scope |
| release and dist (thin LTO, `codegen-units=1`) | blaze has no LTO: it compiles each function, or each unit of `RA_JIT_UNITS`, as a module of its own, and its release programs ran 1.02x to 1.35x of cargo's release time on the bench apps ([research/codegen-units.md](research/codegen-units.md), **read**) |
| Clippy | a rustc driver, which blaze never runs; blaze's own lints are P3 ([roadmap.md](roadmap.md#9-lints), **read**) |
| Linux ARM (`ubuntu-26.04-arm` jobs) | no ARM worker in the fleet; out of scope here |
| Rust built as an input of a JS or Python test | stays rules_rust until an open question below is settled |

**How the two coexist in `ci.yml`.** A `rust-blaze` job runs `cargo blaze test` for Linux x86-64
and macOS arm64 against the server; `linux-test`'s fleet shard and the macOS Rust tests are
removed from Bazel once gate G7 passes. Until then the blaze job runs beside Bazel, not
blocking, and a script compares the two sets of verdicts per test (shadow mode). Bazel keeps
NativeLink for what stays on it; whether Clippy and the release build then use NativeLink or run
hosted-only is tracedecay's choice. The git dependency fetch that took 487.687 s is Bazel's
crate-universe repository rule (**read**); blaze fetches git dependencies itself with their
submodules (`blaze-registry/src/git.rs`, **read**), so the blaze job does not pay that rule,
but it fetches the same submodules once per slot, which G2 measures.

## 8. Gates

Every gate runs at least three times per arm, in the same session, on the named machine, and
reports median, spread and the limiter named from the phase lines (`RA_JIT_STATS`), the
Bazel profile or the server's counters. A speedup is reported only against a baseline measured
in the same session. Unit 1's harness enforces this.

| Gate | Before | Measurement | Bar |
|---|---|---|---|
| **G0** (u1) | everything | blaze builds and tests tracedecay on Linux x86-64 and macOS arm64: cold whole build, warm store, warm remote; functions refused; `building with cargo` lines; peak memory | 0 cargo lines in the job's path; the rest recorded below |
| **G1** | units 5, 8 | unit 2: machine B (other HOME, `CARGO_HOME`, checkout path) builds A's commit from A's pushed packs | 0 functions compiled on ripgrep and nushell; on tracedecay every miss listed with its cause; programs print what cargo's builds on B print, panic paths included |
| **G2** | unit 8's switch | hosted runner, tracedecay test programs from nothing: no remote, warm `Main`, against Bazel's fleet build of the same commit | warm `Main` no slower than Bazel's fleet build; functions compiled only in crates the commit changed |
| **G3** | unit 13's switch | warm slot vs Bazel fleet, same commits: body edit in a leaf crate, body edit in the `tracedecay` crate, signature edit in a crate many depend on | push to verdicts faster than Bazel's fleet job on each, 5 runs each |
| **G4** | unit 17 | cold build of the `tracedecay` workspace into an empty namespace on 1, 2 and 3 slots | 2 slots at least 1.4x faster than 1, 3 at least 1.8x; else mode D stays off and is deleted |
| **G5** | unit 15 | tracedecay's suite on 1 and 3 slots; then 20 runs of an unchanged commit | 3 slots at most 0.6x of 1 slot's wall time; flakes reported by key; 0 tests skipped whose key differs |
| **G6** | units 5, 13 | server: batch lookup of every crate of the build; requests per build | one lookup request per build; 0 per-function requests; server CPU per job recorded |
| **G7** | unit 18's removal of Bazel's paths | two weeks of shadow mode on `master` and pull requests | verdicts equal per test, or each difference filed as a blaze bug and fixed |

G0's numbers, to be filled by u1 (machine, commit, runs, spread):

| tracedecay | Linux x86-64 | macOS arm64 |
|---|---|---|
| builds and tests at all | | |
| `building with cargo` lines | | |
| functions refused | | |
| cold whole build, empty store | | |
| warm store | | |
| warm remote (a directory) | | |
| peak resident memory of the daemon | | |
| test suite wall time, one machine | | |

## Phase plan

Each unit is one pull request against `dist/main`, lands alone, and is checked by its verify
command. Order: measurement before speedup claims, cache before compile, content-only keys
before the cross-machine hits that need them. All harnesses run with `RUST_BACKTRACE=0`; each
worktree has its own `CARGO_TARGET_DIR`. `BIN` below is `target/release/ra-jit` of the unit's
worktree. Paths under `harness/dist/`, the crates `blaze-wire`, `blaze-serve` and `blaze-dist`,
and the files `crates/ra-jit/src/remote/serve.rs` and `crates/ra-jit/src/worker.rs` are new,
made by the unit that first names them; every other path exists at `c8e5f6a8`.

**1. The measurement harness.** Goal: one way to measure every gate. `harness/dist/measure.py`
runs a named scenario (`--scenario`, defined in `harness/dist/scenarios.toml`: workspace,
commit, command, environment, store and remote state before each run) `--runs N` times
(at least 3), takes the phase lines of `RA_JIT_STATS`, functions compiled, store hits, peak
resident memory and bytes moved, and writes one JSON line per run plus a summary with median and
spread. A `--baseline` scenario runs interleaved with the measured one; without one in the same
session it prints no ratio. Touches: `harness/dist/` only. Acceptance: three runs each of
ripgrep's cold and warm-store whole builds give a summary with spread and the largest phase
named. Verify: `cd harness && RA_JIT_BIN=$BIN python3 dist/measure.py --scenario ripgrep-warm --baseline ripgrep-cold --runs 3`.

**2. Cross-machine keys.** Goal: list every reason machine B misses what machine A stored.
`harness/dist/crossmachine.py` makes two machines on one host (own HOME, `CARGO_HOME`,
checkout path, target directory and store, as `remote.py` does), builds on A, pushes to a
directory remote, pulls on B, builds, and prints each function compiled, build script run and
macro expanded on B, grouped by cause (crate id differs, script key, expansion key). A fixture
crate panics with a location, uses `file!()` and has debug info; B's output must be cargo's on
B. Touches: `harness/dist/`, a fixture under `harness/`. Acceptance: ripgrep and nushell, 0
functions compiled on B; tracedecay's misses listed. Verify:
`cd harness && RA_JIT_BIN=$BIN python3 dist/crossmachine.py ~/work/ripgrep -p ripgrep --bin rg`.

**3. The wire schema.** Goal: the types of "Data shapes" and their encoding. New crate
`crates/blaze-wire`: minicbor encode and decode for every message, the section 4.2 validator
(shortest heads, definite lengths, ascending integer keys, no floats), refusal naming the first
offence, and a JSON rendering. Touches: `crates/blaze-wire`, `Cargo.toml`, `Cargo.lock`.
Acceptance: every message round-trips byte-identically; the validator accepts all the encoder
writes on random values and rejects each kind of non-canonical input. Verify:
`cargo test -p blaze-wire`.

**4. The server, today's layout.** Goal: `ra-jit serve --cache DIR --listen ADDR` serving
`hello`, blobs with `Range`, `missing`, and today's `manifests/` over HTTP, and a client place
`Place::Serve` (`blaze+http://host:port`) in `remote.rs`. Push, pull, lazy and the keeper work
unchanged through it. Touches: new `crates/blaze-serve`, `crates/ra-jit/src/remote.rs`,
`crates/ra-jit/src/remote/serve.rs`, `harness/remote.py`. Acceptance: `remote.py` runs its
round trip, damage, signing and kill checks against the server as a third leg. Verify:
`cd harness && RA_JIT_BIN=$BIN python3 remote.py --server`.

**5. Per-crate packs and records.** Goal: decision 2's lookup granularity. A push cuts packs by
home crate and publishes `CrateRecord`s; a daemon computes its `CrateKey`s at load, looks them up
in one batch, and fetches only the packs it lacks. The server merges a crate's small packs.
Touches: `crates/ra-jit/src/remote.rs`, `crates/ra-jit/src/remote/keeper.rs`,
`crates/ra-jit-store/src/lib.rs` (packs sealed per crate), `crates/blaze-serve`. Acceptance:
crossmachine with a server remote: 0 functions compiled on B for ripgrep, one lookup request,
and on a remote holding two workspaces B fetches only its own crates' packs. Verify:
`cd harness && RA_JIT_BIN=$BIN python3 dist/crossmachine.py ~/work/ripgrep -p ripgrep --bin rg --remote server`.

**6. Auth, namespaces and server signing.** Goal: the trust model of decision 2. GitHub OIDC
verification against a configured JWKS, the claims policy file, worker and read tokens,
`Main`/`Pr(n)` namespaces, Ed25519 signing of records by the server, clients trusting its key
through `signed.rs`. Touches: `crates/blaze-serve/src/auth.rs`, `crates/blaze-serve/src/policy.rs`,
`crates/ra-jit/src/remote/signed.rs`, `harness/dist/auth.py` (a local JWKS and token minter).
Acceptance: a pull request token cannot publish to `Main`; a `Pr` record replayed as `Main` is
refused; an expired, unsigned or wrong-audience token is refused; a fork's claims get read only.
Verify: `cargo test -p blaze-serve && cd harness && RA_JIT_BIN=$BIN python3 dist/auth.py`.

**7. GC by live roots.** Goal: decision 2's GC. Root manifests published by `master` builds,
`DELETE /v1/ns/pr/<n>`, expiry of idle namespaces, the byte budget for `Main`, blob grace by the
clock. Touches: `crates/blaze-serve/src/collect.rs`, reusing `remote/collect.rs`'s rules.
Acceptance: after 30 simulated `master` commits and 10 closed pull requests the store is within
budget, and a build at each of the last 20 roots compiles 0 functions. Verify:
`cd harness && RA_JIT_BIN=$BIN python3 dist/gc.py`.

**8. The cache in CI, measured.** Goal: gate G2. `harness/dist/ci/blaze-ci.sh build|test`, which
a workflow calls after joining the tailnet: fetch an OIDC token, set `RA_JIT_REMOTE` and trust,
run `cargo blaze test`, publish a root manifest on `master`. A non-blocking workflow in
tracedecay (its owner's pull request, not this repository) runs it on hosted runners beside
Bazel. Touches: `harness/dist/ci/`, `docs/distributed.md` (results). Acceptance: G2 measured
and written here with runs and spread. Verify: `bash harness/dist/ci/blaze-ci.sh --dry-run`
locally; G2's table filled from the workflow's runs.

**9. Build-script runs keyed by content.** Goal: #105's second item, so a cold machine does not
run every build script. The run key hashes the package's content, its dependencies' content
ids, the environment cargo gives and, on Linux, the digests of every path the script read under
`ra-jit sandbox` (#147's recording), never the script's executable. On macOS a script that reads
outside its package is run again. Touches: `crates/ra-jit/src/cargo/script.rs` (`run_key`),
`crates/blaze-build-script`. Acceptance: crossmachine on nushell, B runs 0 build scripts on
Linux; `harness/keys.py` still 0 open. Verify:
`cd harness && RA_JIT_BIN=$BIN python3 dist/crossmachine.py ~/work/nushell -p nu --bin nu && RA_JIT_BIN=$BIN python3 keys.py`.

**10. Proc-macro expansions keyed by content.** Goal: #105's first item. Checkout-relative cwd,
the environment cargo gives, and the files a macro read under the sandbox in the key; impure
macros stay as `RA_JIT_PROC_MACRO_CACHE_SKIP` says. Touches: `crates/ra-jit-expand`.
Acceptance: crossmachine on nushell, B expands 0 derive macros on Linux; `harness/expansions.py`
passes. Verify: `cd harness && RA_JIT_BIN=$BIN python3 expansions.py && RA_JIT_BIN=$BIN python3 dist/crossmachine.py ~/work/nushell -p nu --bin nu`.

**11. Test verdicts by content.** Goal: ci-runner.md's R2 here. `TestKey` as in "Data shapes";
`tests-reach` and `tests-passed` move from the target directory into a results table of the
local store, then `results/<ns>` on the server; `CARGO_BIN_EXE_*` programs by content.
Touches: `crates/ra-jit/src/cargo/affected.rs`, `crates/ra-jit/src/cargo/sandboxed.rs`,
`crates/ra-jit-store`, `crates/blaze-serve`, a fixture `harness/dist/fixtures/tests` (a pure
test, one that reads a file, one that runs a `CARGO_BIN_EXE_*` program). Acceptance: a test
that passed on machine A is skipped on machine B at the same code; one whose file input changed
runs; `testselect.py` passes. Verify: `cd harness && RA_JIT_BIN=$BIN python3 testselect.py && RA_JIT_BIN=$BIN python3 dist/crossmachine.py dist/fixtures/tests --tests`.

**12. Warm slots.** Goal: decision 3's slot. `ra-jit worker --server URL --repo URL --slots K`
keeps K worktrees, each with a daemon warm on `master`, following it on a timer or on the
server's push, reports `SlotState`, and rebuilds a slot on the triggers listed. The slot
lifecycle is a sans-IO state machine in new `crates/blaze-dist`; the IO driver is
`crates/ra-jit/src/worker.rs`. Touches: those two, `crates/blaze-serve` (worker registry).
Acceptance: on one host, two slots follow a scripted `master`; a new blaze binary rebuilds
them beside the old ones; memory per slot reported. Verify:
`cargo test -p blaze-dist && cd harness && RA_JIT_BIN=$BIN python3 dist/slots.py`.

**13. Jobs on warm slots.** Goal: mode C. `cargo blaze build|test` with `RA_JIT_DIST=URL` sends a
`JobRequest`; the coordinator picks a slot by diff cost; the slot checks out, builds, tests,
publishes and streams events; the client prints what `cargo test` prints. Heartbeats,
reassignment once, cancellation, the no-cargo rule. Touches: `crates/blaze-dist`,
`crates/blaze-serve/src/jobs.rs`, `crates/ra-jit/src/cargo/` (client), `crates/ra-jit/src/worker.rs`.
Acceptance: `harness/dist/jobs.py` on one host with two worker processes: results equal a
local `cargo blaze test`; a killed slot's job finishes on the other; a cancelled job stops; a
job that would go to cargo fails naming why. G3 measured on the fleet. Verify:
`cd harness && RA_JIT_BIN=$BIN python3 dist/jobs.py`.

**14. Local policy and racing.** Goal: `RA_JIT_DIST_LOCAL=never|fallback|race`. Fallback after
the deadline or a remote failure; race starts the local build at once and cancels the loser.
Touches: `crates/ra-jit/src/cargo/` (client), `crates/blaze-dist`. Acceptance: with the fake
fleet stalled, `fallback` finishes locally after the deadline, `race` finishes at local speed
and cancels the remote job, `never` waits and fails with the reason at the deadline. Verify:
`cd harness && RA_JIT_BIN=$BIN python3 dist/jobs.py --policy`.

**15. Test shards and flakes.** Goal: decision 4. Shards by recorded duration over slots of the
platform; binaries as blobs; verdicts published by content; the three-rerun flake rule and its
report line. Touches: `crates/blaze-dist`, `crates/blaze-serve/src/jobs.rs`,
`crates/ra-jit/src/cargo/affected.rs`. Acceptance: a fixture suite with a deliberately flaky
test is reported flaky under its key and does not fail a job that did not change it; G5
measured. Verify: `cd harness && RA_JIT_BIN=$BIN python3 dist/tests.py`.

**16. The Mac slot.** Goal: macOS arm64 jobs on the Mac. A slot with `host_triple`
`aarch64-apple-darwin`; impure tests run every time (no sandbox); executables linked with the
splice's room as the daemon's links do; the job's programs re-signed by the existing Mach-O
path. Touches: `crates/ra-jit/src/worker.rs`, `crates/blaze-dist`. Acceptance: `dist/jobs.py`
on macOS: a job's results equal a local `cargo blaze test` there; a Linux job is never given to
the Mac slot. Verify: `cd harness && RA_JIT_BIN=$BIN python3 dist/jobs.py` on macOS.

**17. Codegen shards.** Goal: mode D, behind `RA_JIT_DIST_SHARDS=N`, off by default. Helper
slots compile instances whose stable-name hash modulo N is their index; the lead builds from the
store. The filter is one predicate in `CodegenUnit::compile` (an instance outside the shard is
expected from the store and waited for). Touches: `crates/hir-ty-codegen/mod.rs`,
`crates/ra-jit/src/main.rs`, `crates/blaze-dist`. Acceptance: G4 measured; the program is the
same as a one-slot build's; the setting is deleted if G4 fails. Verify:
`cd harness && RA_JIT_BIN=$BIN python3 dist/measure.py --scenario tracedecay-shards --runs 3`.

**18. The switch in tracedecay's CI.** Goal: gate G7, then NativeLink's Rust paths removed. A
`rust-blaze` job in shadow mode, a verdict comparison script, then blocking, then `linux-test`'s
fleet shard and the macOS Rust tests removed from Bazel. Touches: `harness/dist/ci/compare.py`
here; tracedecay's `ci.yml`, `.bazelrc` and `docs/ci-remote-builds.md` in its owner's pull
requests. Acceptance: two weeks of equal verdicts or every difference filed and fixed; the
Tailscale grant for `tag:bazel-ci` narrowed to what Bazel still uses. Verify:
`python3 harness/dist/ci/compare.py BAZEL_TEST_LOGS BLAZE_EVENTS`.

### Dependencies outside this plan

- **#344, per-item keys** (blaze-next): unchanged functions keep their keys across an edit, so a
  cold requester hits on a pull request's downstream crates. When it lands, `CrateKey` keeps its
  shape and the per-crate record holds the crate's item-keyed entries.
- **#147, build scripts and proc macros in the sandbox with read recording**: units 9 and 10 take
  their recorded inputs from it. Without it, those units key by content only what is in the
  package and its dependencies, and leave scripts and macros that read outside to run again.
- **#91 (no-rustc K), the cold build without cargo**: slots start with `RA_JIT_WHOLE_BUILD=1` or
  `RA_JIT_RUSTC_BASE=0`; a slot's rustc use is counted, as ci-runner.md counts the seed's.
- **#271, the Merkle VFS**: `SourceRef::Root` replaces git commits, and a slot takes a job as a
  transaction instead of a checkout.

## Open questions

- Is codegen for one target the same on every host, so that identities could drop the host
  `TARGET` and a Linux slot could compile for macOS? Not known; no phase depends on it.
- Do stored functions of registry crates carry `CARGO_HOME` paths into another machine's
  program? Unit 2 answers it before unit 5 relies on it.
- How large is a tracedecay slot, and so how many fit on each fleet machine? G0.
- Are tracedecay's build scripts deterministic, so that two machines agree on crate content ids
  with no shared run records? Unit 2 lists any crate whose id differs.
- What are the fleet machines' core counts and memory, and the hosted runners' sizes? Not in
  tracedecay's files; needed for G2 to G5's limiter.
- The Rust that Bazel builds as an input of JS or Python tests: does it keep building through
  rules_rust, or can Bazel take blaze's programs as prebuilt inputs? Not settled; it decides
  whether tracedecay compiles some Rust twice.
- Clippy after the switch: on NativeLink, or hosted-only? tracedecay's choice.
- The queue the scheduling measurement found had no named limiter (fleet dispatch records were
  missing). Whether warm slots relieve it is G3's question, not a claim here.

## Not checked

- No part of this was built or run. Every bar in the gates is a target.
- The fleet machines, hosted runner sizes and the tailnet round trip were not measured.
- The scheduling measurement is one interrupted run of a release build, which this design does
  not take over from Bazel; it is cited for the shape of the problem, not as a baseline.
- tracedecay was read at `d79de82273`; its CI may have moved since.
