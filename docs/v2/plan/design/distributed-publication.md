---
status: current
date: 2026-11-27
summary: Resolve a unit before granting compilation, retain its accepted output through cancellation, and retire it only after durable delivery.
---
# A unit ends after its full output is published

## Result lifetime and admission

For a complete unit key within one authorized sharing domain, a cache miss is an
observation, not permission to compile. One keyed resolve-or-join operation either returns
an accepted result, attaches to an existing flight, or creates a resolving flight. Only a
cache-absence completion belonging to that flight's current coordinator epoch and lead
attempt enables grants and the full tail. An earlier speculative or direct lookup cannot
provide that authority. Helpers wait for the current lead's resolution.

An optional direct immutable-cache hit avoids a coordinator session. Its miss is discarded
for admission purposes. A newly owned flight performs its own lookup before compilation;
a hit resolves the request without pretending that its unexecuted work set completed.
The agent reaches its compiler callbacks through a private compilation-permit boundary,
and the coordinator's actual join/lease transitions enforce the resolving state as well.

Accepted output has a different lifetime from the execution and its subscribers. The
existing execution record remains the sole in-memory home of the immutable manifest. Its
content index keeps an accepted, undelivered result discoverable even after cancellation,
deadline expiry, lead loss, or terminal-retention expiry. Submit can return that result
without allocating a subscriber or resurrecting an old terminal. Any new requester can
finish delivery after every original session has ended. Exact delivery of an accepted
record may advance publication state after an execution ended, but never changes an
existing subscriber's terminal.

Undelivered result metadata remains subject to the existing execution and byte bounds.
Capacity pressure refuses new cold work; it cannot discard accepted output or prevent
lookup and delivery of the entries consuming that capacity. No live worker or reservation
is required to retain the result. After processing a valid `DeliverPublication`
acknowledgement, ordinary terminal retention may forget the coordinator record. The
adapter sends that acknowledgement only after the catalog's durability-completing
response or verified discovery of the existing durable record. A later flight must
resolve that durable catalog before it can compile.

`CommitPublication` is attempt-fenced acceptance **within the current coordinator
epoch**. `DeliverPublication` acknowledges durable publication in the authoritative
manifest catalog. A coordinator crash between those two boundaries can lose an unmirrored
accepted handle and permit recomputation. Across epochs, work-once reuse begins at durable
catalog publication; an in-memory acceptance is not a restart-stable promise. Losing a
delivery reply does not authorize another compilation in the surviving epoch.

`JobContent` includes its namespace while the immutable cache uses the complete unit key.
Main and PR isolation remains an explicit policy boundary; this design does not promise
globally unique concurrent work across intentionally separate namespaces.

The replaced rule is "initial cache miss plus no live execution permits compilation."
Keeping a second optional check beside that rule would leave accepted-but-unmirrored
output vulnerable to subscriber retirement. A permanent completed-result cache or journal
is unnecessary: pending publication is bounded coordinator state, and the existing durable
manifest catalog remains the one authority after delivery.

## Full output and delivery

An accepted refusal prevents full-manifest commitment and retains a typed refused terminal
under every local policy. Once the full output is accepted, late refusal events and worker
terminal verdicts are rejected without changing that record. An attached agent checks its own
subscription's actual terminal before returning success. A requester receiving an accepted-result
handle checks its own cancellation and request deadline, independently of an old subscriber's
terminal or deadline. Acceptance keeps its original attempt separately
from a replacement lead's delivery attempt; equivalent cache pack partitions must contain
identical key/kind/byte values.

A completed grant phase is not a completed unit. The unit's current lead keeps heartbeating
while the ordinary `Build` finishes its tail, with the committed grant values ahead of its
store. Helpers and subscribers that arrive during this interval wait for the full record.
A unit with no grant names still has one lead completing its tail.

The lead uploads the full output's immutable packs before `CommitPublication`. That call
atomically checks the lead, attempt and completed root set, and accepts one immutable record
inside the coordinator's sans-IO execution. It is the epoch-local acceptance linearization point. The
record survives a lead replacement and cannot be changed by a retry or another lead. The
unit's complete identity covers the inputs of this record; the code generator owns that key.

Cache PUT is delivery of this accepted record, not an authority check. Any authorized requester
holding the accepted result may mirror it and acknowledge `DeliverPublication`. Delivery records
durable publication. A live execution then finishes normally; an already terminal execution and
its subscriber terminals remain unchanged. A lost reply or a dead lead after acceptance
therefore needs delivery, never another compile. A stale attempt can upload immutable blobs,
but cannot commit a new record. A terminal outcome is never resurrected by a late delivery.
The coordinator is ephemeral: restarting it changes its epoch and loses live executions
and unmirrored accepted handles; only the cache mirror is a durable remote result.

The full record contains fresh puts and every successful store read made by the lead's
normal build, with the real value kinds, plus the debug side tables its templates name. A
warm tail must be transferable to a subscriber whose store is empty. Recording keeps only
keys while codegen runs and copies each consumed value once when making the output pack.
This does not add a compiler or a second store.

The rejected ordering was `work done -> Done -> build tail -> publish`. It admitted duplicate
executions while no list was available and let every subscriber compile a separate tail.
A heartbeat followed by an unfenced mutable PUT was also rejected: fencing can change between
those two operations. Attempt-checked acceptance and immutable delivery separate those roles.

Every wait remains bounded by the current request's deadline, including an attached request's
subscription deadline. Neither deadline expires retained accepted-result metadata. Cancellation
leaves an attached subscription and is terminal under every local policy. Missing or corrupt committed packs, malformed published
manifests, ambiguous publication acknowledgements and failed first-writer verification are also
terminal, including under `LocalPolicy::Fallback`. An unavailable carrier before commitment may
follow that explicit local policy. Final compiler and output integrity refusals retain the lead's
original diagnostics and cannot authorize another local build, even if their terminal report or a
concurrent heartbeat fails.

An agent acknowledges its exact stopped assignments after every compiler and heartbeat scope has
returned, including a failed tail. The acknowledgment is accepted only for a pending stop of that
execution and attempt; an old token cannot release a newer reservation. Its original failure is
retained if cleanup loses its carrier.

A first-writer conflict accepts different pack partitions only after verifying identical logical
keys, kinds and bytes; the existing manifest stays immutable. The cache server's conditional
repair API and durability simulation remain available for explicit repair of absent packs, but
an agent never recompiles or repairs a published absence as a cache-miss recovery path.

## Proof and cost

The admission simulation drives the actual coordinator and job transitions with tape-drawn
cache completion, cancellation, deadline, replacement, reply-loss and restart schedules.
Its oracle counts full-tail starts by complete result identity across execution IDs. It must
cover a negative lookup delayed across durable completion, a stale attempt's lookup reply,
last-subscriber cancellation before mirroring, terminal expiry before and after delivery,
restart after durable publication, and delivery after all original sessions have ended or
retention capacity is full. Planted controls bypass resolution authority, forget an
undelivered result, or require a live subscriber for delivery; each must produce a failing
replayable tape. The existing early-Done and late-verdict publication controls remain.
Forced-order carrier tests check the real HTTP/Unix adapters and exact slot cleanup after
that simulation, including committed missing-pack refusal without compilation.

Let P be manifest metadata size, B the result bytes a consumer needs, and S the number of
readers. These are design budgets, not measured performance claims.

| Operation | Required work |
|---|---|
| Direct published hit | One authoritative lookup and O(P + B) consumption; no execution, grant, or tail |
| Join unfinished flight | One keyed lookup and bounded subscriber bookkeeping; no independent full tail |
| Cold flight | One authoritative lookup under current ownership, then one full build and publication; the optional direct-hit shortcut adds one explicitly counted cold GET |
| Stale miss after completion | Resolve the accepted or durable result; zero full-tail work |
| Accepted output awaiting delivery | O(P) control work and required delivery I/O; no compilation or live worker requirement |
| Cancellation | Bounded subscription and exact-assignment operations; no output-byte scan |
| Restart after durable completion | Resolve the durable catalog; no historical-execution reconstruction |
| S readers | O(S) interest/delivery bookkeeping and each reader's necessary bytes; no S-times-tail term |

End-to-end cold and edited builds must measure the extra lookup against the repository's
1.1-times/unavoidable-I/O budget. No latency ratio is established by this design review.

## Independent architecture review

The rule-66 review read the committed implementation and Buildbarn at
`65cbecb86a0fa6ba39a99e457d7ced267d529c69`: its
[scheduler](https://github.com/buildbarn/bb-remote-execution/blob/65cbecb86a0fa6ba39a99e457d7ced267d529c69/pkg/scheduler/in_memory_build_queue.go),
[caching executor](https://github.com/buildbarn/bb-remote-execution/blob/65cbecb86a0fa6ba39a99e457d7ced267d529c69/pkg/builder/caching_build_executor.go),
[storage flushing executor](https://github.com/buildbarn/bb-remote-execution/blob/65cbecb86a0fa6ba39a99e457d7ced267d529c69/pkg/builder/storage_flushing_build_executor.go),
[worker wiring](https://github.com/buildbarn/bb-remote-execution/blob/65cbecb86a0fa6ba39a99e457d7ced267d529c69/cmd/bb_worker/main.go), and
[build client](https://github.com/buildbarn/bb-remote-execution/blob/65cbecb86a0fa6ba39a99e457d7ced267d529c69/pkg/builder/build_client.go).
Those paths separate caller interest from shared work and flush outputs before publishing
the action result and reporting success. Their live-task deduplication does not establish
Blaze's stronger stale-miss invariant. The review approved retaining fenced full-output
publication and replacing admission/result lifetime with the design above, including the
explicit epoch-local acceptance boundary and the required actual-core simulation.

An agent that leaves while other subscribers remain requests its owned assignment's retirement
before acknowledging its stop. The session and attempt fences, the two-step capacity release,
and the pending runtime checks are specified in
[assignment retirement](distributed-retirement.md). An unmatched acknowledgment remains refused.
