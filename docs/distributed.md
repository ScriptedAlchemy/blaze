---
status: current
summary: The retained distributed protocol, coordinator, pack cache and test controls; compiler integration awaits callbacks into the current compiler.
---
# Distributed protocol and cache

The fork retains the distributed state machines, Unix transport, agent library and
standalone pack server. They are in the active workspace and can be exercised without a
compiler. The old compiler daemon and its distributed bridge were deleted by the upstream
rewrite. The `blaze` executable is now built by `blaze-cli`; `blaze-engine` is now a library
for the new execution engine. Neither restores the removed distributed bridge. The
retained services need compiler callbacks on the current item and command layers before
distributed compilation works end to end. The [deletion record](v2/plan/parked.md)
describes which old implementations were removed.

This page describes the surviving implementation. The
[publication design](v2/plan/design/distributed-publication.md),
[assignment retirement design](v2/plan/design/distributed-retirement.md) and
[work identity design](v2/distributed-work-identity.md) retain the compiler integration's
contracts.

## What remains available

| Component | Responsibility |
|---|---|
| `crates/blaze-dist` | Sans-IO admission, executions, leases, work results, publication and terminal state machines. |
| `crates/blaze-dist-local` | Coordinator and client over Unix sockets, an agent accepting compiler callbacks, pack transfer and transition evidence. It compiles no Rust itself. |
| `crates/blaze-wire` | The shared deterministic CBOR schemas, typed identities and terminal contracts. |
| `crates/blaze-serve` | HTTP/1.1 server for immutable packs, manifests and accepted publication records. |

The standalone binaries keep these interfaces:

```sh
blaze-dist-local coordinate SOCKET TRIPLE EXECUTORS [LEASE_MS]
blaze-serve --dir DIR --listen 127.0.0.1:PORT
```

The coordinator advertises executor slots for the supplied host triple and runs until its
stdin closes or receives `stop`. Optional `LEASE_MS` sets the holder silence bound; otherwise
the work configuration's default applies. Running it does not start compiler workers.
A library caller supplies `agent::Config`, canonical `agent::Unit` names, and an
implementation of `agent::Granted` for compiling grants and completing a unit. The socket,
cache address, namespace, local policy and finite execution deadline are explicit fields
of that configuration.

The pack server has no authentication or collection. Keep a local instance on loopback;
a shared deployment needs an authenticated boundary. Its stored files remain until
removed. This server and the signed object-bucket segment cache have different formats
and trust boundaries. The latter is documented in [shared-cache.md](shared-cache.md).

## A unit through the agent library

The implementation is `crates/blaze-dist-local/src/agent.rs`. The caller, not the carrier,
defines the compiler work and its keys.

1. If a published list exists for the unit, the agent reads and verifies its packs without
   scheduling work. A committed pack that is missing or corrupt is a terminal error.
2. Otherwise it submits the unit. Equal job content joins an existing execution. An agent
   claims its assigned executor, joins the canonical work set and receives leases for
   batches. Its supplied compiler callback makes or retrieves the values. The agent
   uploads each pack before reporting the batch's result.
3. The current lead reads the committed phase values and calls the supplied completion
   callback once for the final unit work. It uploads the remaining values and submits a
   full manifest. The coordinator accepts that manifest against the live lead attempt
   before any cache mirror can establish publication.
4. Helpers and late subscribers take the accepted full manifest. Publication identity
   does not change when a later attempt delivers the accepted record. Accepted output
   cannot be replaced by compiling a different tail.

Ownership is bound to the transport session. Retirement fences an owned assignment and
requests its stop; its reservation is released only after a matching stop acknowledgment.
Cleanup can find the current token of the same owned slot after promotion, without
claiming another slot. These transitions are implemented in the carrier and core, with
their invariants stated in the [retirement design](v2/plan/design/distributed-retirement.md).

The former compiler bridge supplied the real code generator, store import and final
link/export callbacks. Those connections are absent from the current tree. An agent test
with a stand-in compiler exercises this protocol but does not restore that bridge.

## When something fails

`agent::decide` applies the caller's explicit `LocalPolicy`. The retained policy is:

| Event | `Never` | `Fallback` |
|---|---|---|
| A valid terminal verdict | Take the verdict. | Take the verdict. |
| An execution ends without a verdict, as permitted by the terminal contract | Fail. | Permit the caller to build locally. |
| Coordinator or transport failure before committed publication | Fail. | Permit the caller to build locally. |
| Missing or corrupt committed pack, or failure delivering an accepted publication | Fail. | Fail. |
| Final compiler or output-export refusal | Return the original refusal. | Return the original refusal. |
| Cancellation by this caller | Cancel and retire its owned assignment. | Cancel and retire its owned assignment. |

Permitting a local build is a decision returned to the caller; the protocol crate has no
alternative compiler to invoke. The immutable terminal is checked against the exact
subscription and attempt. A pending or mismatched terminal is not a successful result.

Main and pull-request namespaces keep executions separate. Admission and revision
replacement are core protocol transitions; identical published unit content can still
be reused across those executions. The current library receives namespace and source
identity through `agent::Config`, rather than deriving them from a workspace itself.

## Environment variables and acceptance probes

Only the two variables in the first table below are read by the retained carrier's
runtime code. They request evidence or an explicit acceptance fault; they do not enable
distributed compilation in the current CLI.

| Variable | Current behavior |
|---|---|
| `BLAZE_DIST_EVENTS` | Path for append-only JSON lines of actual agent and coordinator transitions. Each append holds a file lock, with a two-second lock bound, so processes do not interleave records. Write failure is logged as a warning; the acceptance harness separately rejects missing evidence. With the variable absent, no event file is opened. |
| `BLAZE_DIST_TEST_RESULT_GATE` | Acceptance-only path to a fresh, already-created, empty absolute directory. The agent enables this only for a current lead and supplies a positive finite timeout. It captures one real result and pauses that sender's heartbeats until a `release` file appears or the deadline expires. Normal request input does not select the gate. |

The result gate uses exclusive creation of `claimed` to select one result across callers.
It writes the real canonical command to `result.cbor` and an accompanying `ready.json`
with its lease fields and digest. Time spent compiling before capture, and time spent
stopped, consume the timeout. On release, the same command goes through the normal client
to the real coordinator; no lease field is changed and no authority is granted by the
gate. The response or error is recorded. A stale claim is not erased or reused.
With no gate configured, the driver makes no capture and performs no gate file IO.
Implementation and tests are in `crates/blaze-dist-local/src/result_gate.rs`,
`crates/blaze-dist-local/src/result_gate/tests.rs` and
`crates/blaze-dist-local/src/driver.rs`.

The retained end-to-end harness still supplies the previous compiler bridge's input
contract. These variables are present in `harness/distv2.py`; the deleted daemon bridge
was their compiler-side reader, so setting them on the current CLI is not a
working setup procedure:

| Harness variable | Contract to restore with compiler integration |
|---|---|
| `BLAZE_DIST_SOCKET` | Coordinator Unix socket. |
| `BLAZE_DIST_BLOBS` | Pack-server address, `host:port`. |
| `BLAZE_DIST_LOCAL` | `never` or explicit `fallback` policy. |
| `BLAZE_DIST_PR`, `BLAZE_DIST_SOURCE` | Pull-request namespace and revision identity. |
| `BLAZE_DIST_REPORT` | Per-unit JSON provenance used to prove where compiled and reused values came from. |

The simulation tests have their own controls:
`BLAZE_DIST_SEEDS` changes the seeds per invariant set (default 10,000), and
`BLAZE_DIST_SEED` chooses one seed for the `replay` test. They are read only in
`crates/blaze-dist/tests/sim.rs`, not by the coordinator or a compiler.

## Wire format and calls

The codec's types and discriminants live in `crates/blaze-wire`; the local command
envelopes live in `crates/blaze-dist-local/src/protocol.rs`. The
[carrier reference](../crates/blaze-dist-local/README.md) documents framing, version
negotiation, session creation and resumption, command bindings, replay and limits.

A frame is a four-byte big-endian body length followed by one deterministic CBOR value.
Noncanonical encoding, unknown fields or variants, trailing bytes, oversized frames and
version mismatches are refused. Session epochs and sequence numbers bind a response to
its request. Retrying an admitted request uses the recorded answer and must not repeat
its core effects. These identities do not themselves grant compiler-work authority;
claims, lease generations and attempts supply that authority.

The pack server's endpoint and storage contracts are in
`crates/blaze-serve/src/lib.rs` and `crates/blaze-serve/src/server/publication.rs`.
Packs are named and checked by their content hash. Publication acceptance belongs to the
coordinator; the server mirrors accepted records and checks delivery. It is not a remote
compiler or a remote transport for the Unix coordinator.

## Data shapes

These are the shared version-5 schema types in `crates/blaze-wire/src/shapes.rs` and
`crates/blaze-wire/src/calls.rs`, retained by the protocol library. They describe encoded
values; their presence does not imply a working compiler bridge or an HTTP coordinator.
The complete inventory is `blaze_wire::vectors::every`, with golden vectors under
`crates/blaze-wire/tests/golden/`.

A struct is a CBOR map of the numbered fields below, every key present. An enum is the
array `[discriminant, payload]`; a variant without fields has a null payload. `Digest` is
a 32-byte CBOR byte string. A `Set<T>` is a sequence ordered by each item's canonical CBOR
bytes, with duplicates removed when encoded and refused when decoded. `Vec<T>` preserves
its order. An `Option<T>` is its value or null, and a tuple is an array of its fields.
Digests are `sha256("blaze/" ‖ name ‖ "/v5" ‖ 0x00 ‖ CBOR)`.
Comments give map keys and enum discriminants.

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
// The input of a WorkName: a stable item or an edge from another work item.
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
// Stages carried by timestamped events.
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

The local carrier's command and response envelopes use these shared values where their
schema calls for them. Session creation, bindings, retirement, publication commands and
the separately versioned terminal contract are documented in the
[carrier reference](../crates/blaze-dist-local/README.md). The declarations below specify
field numbers and values; they do not define HTTP endpoints.

```rust
// A unit on one platform. Fields 0 to 2 identify shared content; 3 to 5 belong to its subscriber.
struct JobRequest {
    unit: Digest, // 0
    platform: Digest, // 1
    namespace: Namespace, // 2
    source: Digest, // 3
    deadline_ms: u64, // 4
    local: LocalPolicy, // 5
}
// A submitted job's shared content, execution and subscriber.
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
// An executor's work-set join.
struct WorkJoinRequest {
    execution: ExecutionId, // 0
    attempt: Attempt, // 1
    roots: RootSet, // 2
    rejected: Set<WorkName>, // 3
}
// The agreed roots and whether they are fixed.
struct WorkJoinAnswer {
    roots: RootSet, // 0
    fixed: bool, // 1
}
// A request for work in an execution's attempt.
struct WorkLeaseRequest {
    execution: ExecutionId, // 0
    attempt: Attempt, // 1
}
// A grant, or null when no work is granted.
struct WorkLeaseAnswer {
    grant: Option<Grant>, // 0
}
// The names granted or refused as conflicts after a batch result.
struct WorkResultAnswer {
    granted: Set<WorkName>, // 0
    conflicts: Set<WorkName>, // 1
}
// Events reported by an executor in an execution's attempt.
struct WorkersEventsRequest {
    execution: ExecutionId, // 0
    attempt: Attempt, // 1
    events: Vec<JobEvent>, // 2
}
// The worker verdict: 0 to continue, 1 to cancel.
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

The separate pack server's endpoint contracts are in `crates/blaze-serve/src/lib.rs`.

## Tests and the remaining acceptance boundary

`crates/blaze-dist/tests/sim.rs` exercises the sans-IO cores deterministically, including
lost workers and fencing. `crates/blaze-dist-local/tests/agent.rs` uses real sockets and
the pack server with a stand-in compiler; it covers split work, cached units, cancellation,
lead completion, publication and refusal behavior. The carrier's other tests cover wire
and session lifetimes. `crates/blaze-serve/tests/serve.rs` exercises the HTTP server.

`harness/distv2.py` retains real-compiler acceptance cases: two workers on one workspace,
published reuse, edits, separate PR namespaces, killed or stopped workers, stale results,
damaged packs, coordinator loss and explicit local policies. It verifies actual output
and compilation provenance. `harness/distv2/selfcheck.py` checks the evidence oracles with
planted faults; those checks are not a substitute for running the compiler cases.

The required jobs in `.github/workflows/blaze.yaml` separate workspace and protocol tests,
source-compiler smoke checks, and production distributed acceptance. They require a
verified compiler built from the current source and complete dev/release acceptance
receipts. The library suite provides partial validation while the compiler bridge is absent.
It does not satisfy the compiler build or distributed acceptance requirements, and an
absent receipt, skipped job or failed subprocess is not a pass.
