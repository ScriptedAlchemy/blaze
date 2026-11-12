---
status: current
summary: A build across several daemons on V2's one build: executors, leases, the shared cache of packs, the requester's fallback and the wire.
---
# Distributed compile and cache

Several daemons can build one workspace together and keep what they compile in one cache. There
is one compiler pipeline, the build every daemon runs anyway: the distributed path decides who
compiles which function, and puts the results in front of that build's store as hits under the
same keys. It adds no second code generator, no second store and no other tool: every executor
is a daemon running blaze ([no-rustc.md](no-rustc.md)).

Work can be a standalone dev template, a release compile with its unit's import decisions, or a
complete folded group. Each executor computes the same work set before taking a grant; release
imports and every member of a folded group are part of that task's identity. The unit's published
list lets later builds reuse the outputs without scheduling those tasks again.

The fork's earlier design for V1, with its saved build specifications, signed records and
distributed tests, is kept as a [record](reports/DISTRIBUTED-V1-DESIGN.md); none of it applies here.

## Using it

A request names the path in its environment:

| Variable | What it says |
|---|---|
| `BLAZE_DIST_SOCKET` | the coordinator's Unix socket; with it set, every unit goes this way |
| `BLAZE_DIST_BLOBS` | the cache server, `host:port`; required with the socket |
| `BLAZE_DIST_LOCAL` | `never` (the default) or `fallback`: what a failure of the path means |
| `BLAZE_DIST_DEADLINE_SECS` | how long a unit may wait for its execution; an hour by default |
| `BLAZE_DIST_PR`, `BLAZE_DIST_SOURCE` | the pull request this build is of, and which revision |
| `BLAZE_DIST_REPORT` | a file each unit appends one line of JSON to, saying where its code came from |

The coordinator and the cache server are programs of their own:

```sh
blaze-dist-local coordinate SOCKET TRIPLE EXECUTORS
blaze-serve --dir DIR --listen 127.0.0.1:PORT
```

## How a unit goes

`crates/blaze-cargo/src/serve/unit/dist.rs` wires it into a unit's codegen, and
`crates/blaze-dist-local/src/agent.rs` is one daemon's side of it.

1. The unit's walk names its work. Equal instance keys are equal inputs, whatever machine computed
   them, so the digest of a walk's keys (`blaze_codegen::driver::WorkSet::digest`) is the job:
   executors that differ in anything a function's code depends on are not in one execution.
2. A list published under that digest names everything an earlier build made. Its packs are read,
   each checked against what the list says, and nothing is scheduled. A list naming a pack the
   server lost is an unpublished unit.
3. Otherwise the agent submits the job. An equal job joins the running execution. It claims the
   executor assigned to it and runs a holder: join, lease, compile a batch of functions by the code
   a store miss runs (`Build::compile_granted`), seal them into a pack, upload it, then report it,
   so a committed result's bytes exist. Heartbeats go on from another thread, since the batch
   compiler owns its thread.
4. When the work is done every agent reads every committed pack. The build then runs as always,
   with those values in front of its store. `BLAZE_VERIFY_STORE` samples them as it samples the
   store's own; what nothing committed it compiles itself and reports.
5. What it took goes into its own store with what it made, so the link, the record and the next
   build have one store to read, and the unit's list is published for the builds to come.

The coordinator retains a completed compile phase through final export and publication. A
request for the same unit during that interval joins the execution and waits for its immutable
manifest; it does not start another compile. Publication acceptance and delivery are separate,
attempt-fenced transitions. The implementation and pending runtime evidence are described in
[distributed publication](v2/plan/design/distributed-publication.md).

## When something fails

A holder that stops heartbeating has its lease expire and its functions granted again. A lead that
misses three heartbeats loses the job, which is reassigned once with a new attempt; the dead
attempt's late results are refused whole (`Refusal::AttemptMoved`, `Refusal::DeadLease`), so a
stale worker cannot commit. A second loss ends the execution.

A batch that fails on an executor is given back, and the final build compiles those functions and
reports any real error. When the path itself fails, `agent::decide` says what the requester does:

| What happened | `never` | `fallback` |
|---|---|---|
| a verdict was reported by the execution | taken | taken |
| the execution ended without one (no lead, lost twice, cancelled, past its deadline) | the build stops, naming it | compiled here |
| the coordinator or cache server is unavailable before commitment | the build stops, naming it | compiled here |
| the final compiler or output exporter refuses the unit | the build stops with the original diagnostic | the build stops with the original diagnostic |
| committed packs are missing or corrupt, or publication delivery fails after acceptance | the build stops, naming it | the build stops, naming it |

Compiled here is the same build the daemon runs alone. A cancelled build leaves its execution, and
an execution ends when its last subscriber leaves. A caller's own cancellation is terminal under
both policies. A full manifest accepted by the coordinator is immutable: a later worker failure
or refusal cannot replace it or turn it into permission to compile again.

After its local compiler and heartbeat scopes stop, an agent retires its owned assignment and
separately acknowledges the resulting stop. This releases a departing helper's reservation while
other subscribers continue. A read of its exact owned slot finds a promotion that the agent did
not observe before cancellation, without claiming new work. Both stop requests name the exact
execution and attempt; retirement also binds the claimant to its transport session. An old
request cannot stop or release a newer assignment. The protocol and pending runtime checks are in
[assignment retirement](v2/plan/design/distributed-retirement.md).

## Pull requests and the shared cache

A pull request's jobs are its own executions, on executors of its namespace, and they are
admitted after the main line's. What any build published is shared by content: a pull request's
daemon that reaches a unit another pull request or the main line published takes its packs and
compiles none of it.

A build is many jobs, so two revisions' builds overlap. The first job of a newer
`BLAZE_DIST_SOURCE` cancels the older revision's executions that nothing else waits for, and a job
of a source its pull request has replaced is refused (`Refusal::Superseded`).

## The signed global cache

`blaze-global` has a separate signed-record and bundle client over TLS. Its library tests
exercise publication and an empty-store download through the real TLS channels, with controls
for an untrusted certificate, a wrong server name, an untrusted or damaged manifest signature,
an untrusted standalone value signature, and a value that differs from its signed manifest.
A trusted manifest authorizes its exact listed records. The network and disk completions come from
`blaze-core`'s seeded simulation. The required source-built protocol job runs these tests.

Overlapping bundle downloads track all of their missing values, including a fetch another
owner already started. A shared completion is read by every waiting download without being
consumed by the first one; each observes the original outcome and retains a refusal or absence.
A download cannot report completion merely because it did not start a new request itself.
The driver broadcasts each drained `Store::fetched` batch to existing owners before
delivering new manifests or scheduling a retry for the same key. Outcomes identify a key,
not a fetch generation; delivering an old batch after creating a new owner could settle
the new download with the previous attempt's outcome.

A readable value or successful shared fetch is checked against that download's complete
signed record: key, kind, content digest and byte length. `Store::get` supplies the value
under the store's existing content-checking contract; the bundle layer compares its metadata
without rehashing the payload. A self-consistent local value accepted by another owner can
still be the wrong record for this manifest and must fail the download. This check applies
both before scheduling and to values published during scheduling or returned by another
owner's fetch.

Readability and durability are separate. A successful download returns the durability tickets
for fetched values and for already-readable puts still awaiting sync when the manifest arrives.
It also handles a value becoming readable while fetches are being scheduled. The caller must
wait for those tickets before claiming durability; a readable value with a held sync is not a
durable bundle. The simulation fixtures hold and release sync completions and reopen after a
crash to check that boundary.

Verified manifests may repeat an identical key-to-record binding. A later manifest cannot
replace that binding with another record: the client checks the whole manifest for conflicts
before adding any entries, so refusing one record leaves every previous binding intact and
adds none of the refused manifest's other records. The bundle models and their planted
controls are in `crates/blaze-global/src/tests/download.rs`: four Hegel models and ten
planted controls. The Stage 1 receipt requires each to report success in the complete test
run. Hosted receipts record their results; their source presence does not settle D11.

The compiler daemon does not yet drive that bundle client, and `blaze-global::PINNED` has no
publisher keys. A trusted service and publisher policy, daemon integration, a real cached-tree
build, and causal evidence that remote IO stays off the critical path are still needed for D11.
The distributed pack-cache acceptance described above does not establish those properties.

## Data shapes

Every value on the wire is deterministic CBOR: a struct is a map of small integers with every key
present, an enum the array `[discriminant, payload]`. `crates/blaze-wire` holds them, with golden
vectors under `crates/blaze-wire/tests/golden/`. Digests are
`sha256("blaze/" ‖ name ‖ "/v5" ‖ 0x00 ‖ CBOR)`. Comments give field numbers.

```rust
// Where a local build looks before it compiles.
enum LocalPolicy {
    Never, // 0
    Fallback, // 1
}
// The namespaces of the cache, and what may write each.
enum Namespace {
    Main, // 0
    Pr(PrNamespace), // 1
}
// The fields of `Namespace::Pr`: its pull request's number.
struct PrNamespace {
    n: u64, // 0
}
// One pack of a crate's record.
struct PackRef {
    pack: Digest, // 0
    size: u64, // 1
    keys: Digest, // 2
    packed: Option<(Digest, u64)>, // 3
    format: u32, // 4
}
// A function of distributed codegen, as every executor of an execution names it: the input of its `
enum WorkKey {
    Item(ItemKey), // 0
    Edge(EdgeKey), // 1
}
// The fields of `WorkKey::Item`: a function with a stable name.
struct ItemKey {
    unit: Digest, // 0
    stable: Digest, // 1
    env: Digest, // 2
}
// The fields of `WorkKey::Edge`: a request with no stable name.
struct EdgeKey {
    parent: WorkName, // 0
    ordinal: u32, // 1
}
// A name an executor found, and the digest of the complete local request it holds for it.
struct Found {
    name: WorkName, // 0
    tuple: Digest, // 1
}
// What an executor computed from its front end before it may take work.
struct RootSet {
    spec: Digest, // 0
    roots: Digest, // 1
    count: u64, // 2
    index: Digest, // 3
    inputs: Digest, // 4
}
// An executor's tenure over the work it holds in one execution.
struct Lease {
    execution: ExecutionId, // 0
    attempt: Attempt, // 1
    holder: String, // 2
    generation: u64, // 3
}
// Work given to a holder: a range of the sorted roots, and names granted to it.
struct Grant {
    lease: Lease, // 0
    roots: Set<(u64, u64)>, // 1
    names: Set<WorkName>, // 2
}
// What an executor's batch did with one name.
enum ItemOutcome {
    Exported(EntryRef), // 0
    FromStore(EntryRef), // 1
    Failed(Why), // 2
    Local(Why), // 3
    Lost(Why), // 4
}
// The fields of `ItemOutcome::Exported` and `ItemOutcome::FromStore`.
struct EntryRef {
    entry: EntryKey, // 0
}
// The fields of `ItemOutcome::Failed`, `Local` and `Lost`.
struct Why {
    why: String, // 0
}
// One name's result in a batch.
struct ItemResult {
    name: WorkName, // 0
    outcome: ItemOutcome, // 1
    found: Set<Found>, // 2
    unnamed: Set<(u32, String)>, // 3
}
// One batch's results, committed at once.
struct BatchResult {
    lease: Lease, // 0
    seq: u64, // 1
    items: Vec<ItemResult>, // 2
    packs: Vec<PackRef>, // 3
    returned_roots: Set<(u64, u64)>, // 4
    returned_names: Set<WorkName>, // 5
}
// What admission holds for an executor, a job or a test shard.
struct Reservation {
    memory: u64, // 0
    cpus: u32, // 1
    disk: u64, // 2
}
// Stage timestamps, research 7.12.
enum Stage {
    Queued, // 0
    Admitted, // 1
    Assigned, // 2
    Fetched, // 3
    Loaded, // 4
    Compiled, // 5
    Published, // 6
    Linked, // 7
    Reported, // 8
    Done, // 9
}
// One event of an execution's stream.
enum JobEvent {
    Stage(StageEvent), // 0
    Assigned(AssignedEvent), // 1
    Refused(RefusedEvent), // 2
    Output(OutputEvent), // 3
    Done(DoneEvent), // 4
}
// The fields of `JobEvent::Stage`.
struct StageEvent {
    stage: Stage, // 0
    at_ms: u64, // 1
}
// The fields of `JobEvent::Assigned`.
struct AssignedEvent {
    executor: String, // 0
    attempt: Attempt, // 1
}
// The fields of `JobEvent::Refused`.
struct RefusedEvent {
    function: String, // 0
    why: String, // 1
}
// The fields of `JobEvent::Output`.
struct OutputEvent {
    stream: u8, // 0
    bytes: Vec<u8>, // 1
}
// The fields of `JobEvent::Done`.
struct DoneEvent {
    outcome: Outcome, // 0
    counters: Vec<(String, u64)>, // 1
}
// How an execution ended.
enum Outcome {
    Passed, // 0
    Failed, // 1
    Refused, // 2
    Lost, // 3
    Cancelled, // 4
    TimedOut, // 5
}
// The input of `JobContent`'s digest.
struct JobContentInput {
    unit: Digest, // 0
    platform: Digest, // 1
    namespace: Namespace, // 2
}

// Newtypes over a digest or a number, each its inner value on the wire.
struct EntryKey(Digest);
struct JobContent(Digest); // of a JobContentInput
struct WorkName(Digest); // of a WorkKey
struct ExecutionId(u64);
struct Attempt(u32);
```

## The calls

The carrier is a Unix socket: a four-byte big-endian length, then one CBOR value
(`crates/blaze-dist-local/README.md`). `crates/blaze-dist-local/src/protocol.rs` wraps these with
the sender's identity and adds `Claim`, which says which assigned executor an agent runs, and
`Packs`, which says what the compile phase committed. A session has one request outstanding at a
time and keeps the answer to the last, which a retry after a lost connection gets again byte for
byte, so one that polls for hours holds one answer, not every one.

```rust
// A job: one unit of a build on one platform. Fields 0 to 2 are its content, what equal jobs share 
struct JobRequest {
    unit: Digest, // 0
    platform: Digest, // 1
    namespace: Namespace, // 2
    source: Digest, // 3
    deadline_ms: u64, // 4
    local: LocalPolicy, // 5
}
// `POST /v1/jobs`'s answer.
struct JobAnswer {
    content: JobContent, // 0
    execution: ExecutionId, // 1
    subscriber: u64, // 2
}
// One item of an event stream, each behind its four-byte big-endian length.
struct EventItem {
    sequence: u64, // 0
    attempt: Attempt, // 1
    event: JobEvent, // 2
}
// `POST /v1/work/join`'s request.
struct WorkJoinRequest {
    execution: ExecutionId, // 0
    attempt: Attempt, // 1
    roots: RootSet, // 2
    rejected: crate::Set<WorkName>, // 3
}
// `POST /v1/work/join`'s answer.
struct WorkJoinAnswer {
    roots: RootSet, // 0
    fixed: bool, // 1
}
// `POST /v1/work/lease`'s request.
struct WorkLeaseRequest {
    execution: ExecutionId, // 0
    attempt: Attempt, // 1
}
// `POST /v1/work/lease`'s answer: a grant, or `null` for nothing.
struct WorkLeaseAnswer {
    grant: Option<Grant>, // 0
}
// `POST /v1/work/result`'s answer.
struct WorkResultAnswer {
    granted: crate::Set<WorkName>, // 0
    conflicts: crate::Set<WorkName>, // 1
}
// `POST /v1/workers/events`'s request.
struct WorkersEventsRequest {
    execution: ExecutionId, // 0
    attempt: Attempt, // 1
    events: Vec<JobEvent>, // 2
}
// `POST /v1/workers/events`'s answer.
struct WorkersEventsAnswer {
    verdict: u8, // 0
}
// The one shape an error takes on every call.
struct WireError {
    code: u8, // 0
    message: String, // 1
    retry_after_ms: Option<u64>, // 2
}

// The answer of every call that has nothing to say: `{}`.
struct Ack {}
```

The cache server's calls are in `crates/blaze-serve/src/lib.rs`.

## What it does not do

- **One machine's socket.** The coordinator's transport is a Unix socket, so executors on other
  machines need it forwarded; no remote carrier is built.
- **No authentication and no collection** on the cache server: it listens on loopback unless told
  otherwise, and its directory grows until it is removed.
- **Distributed tests** are not wired: `blaze test` runs here, whatever the build did.

## How it is tested

`crates/blaze-dist` holds the sans-IO state machines and a deterministic simulation of them,
including fencing and lost workers. `crates/blaze-dist-local/tests/` runs them over real sockets,
with the agent and a stand-in compiler against the real cache server.
`harness/distv2.py` runs real daemons on a real workspace: two workers compiling one build, a
third daemon reusing what they published, an edit, another pull request's checkout reusing it, a
worker killed mid-build, a worker stopped past its lease and resumed, a damaged pack, a lost
coordinator with and without `fallback`, and the programs each daemon linked run and compared
byte for byte.

The Linux AArch64 lane of `.github/workflows/blaze-platform.yaml` requires this real harness after
its current-tree build. It verifies the build artifact's commit, tree and binary SHA-256 identities,
uses that Blaze to build the coordinator and cache server in an isolated home, and runs both dev
and release with faults enabled. A separate job uses the same verified source-built compiler to
run the protocol and carrier crates' library and integration tests with their Hegel controls;
the commands and their final stop run in one fresh home under the Linux
[kernel execution observer](harness.md#kernel-execution-observer). The merge gate requires
both named test groups, their exact-line control checks, unchanged source and binaries, and
a complete kernel-execution receipt. The pinned bootstrap predates the compiler repair
needed by packages whose library and binary share a name, so it cannot supply this receipt.
Release also requires two holders to emit actual LLVM and folded
work. The workers have private stores and targets on the runner's verified reflink filesystem;
each receives two cores and a 6 GiB daemon memory hint. These are resource hints, not whole-process
memory limits. CI retains inputs, identities, logs, reports, lease/result captures and artifact
hashes on failure as well as success. The aggregate check requires both complete runs; a missing
receipt, skipped job or failed subprocess cannot count as acceptance. The separate
`harness/distv2/selfcheck.py` step checks planted bugs in the evidence oracles only.
