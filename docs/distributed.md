---
status: current
summary: The retained distributed protocol, coordinator, pack cache and test controls; compiler integration awaits the replacement command and item layers.
---
# Distributed protocol and cache

The fork retains the distributed state machines, Unix transport, agent library and
standalone pack server. They are in the active workspace and can be exercised without a
compiler. The old compiler daemon and its distributed bridge were deleted by the upstream
rewrite. A current-tree compiler must be connected to the replacement item and command
layers before distributed compilation works end to end. The
[parked-crate status](v2/plan/parked.md#distributed-fork-integration) records that boundary.

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
distributed compilation in the parked CLI.

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
was their compiler-side reader, so setting them on the current parked CLI is not a
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

The distributed jobs in `.github/workflows/blaze-platform.yaml` still require a verified
compiler built from the current source and complete dev/release acceptance receipts.
The library suite is useful partial validation while the command layer is parked.
It does not satisfy the compiler build or distributed acceptance requirements, and an
absent receipt, skipped job or failed subprocess is not a pass.
