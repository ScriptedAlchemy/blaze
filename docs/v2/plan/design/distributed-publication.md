---
status: current
summary: One attempt-owned distributed unit through its full output record, followed by idempotent cache delivery.
---
# A unit ends after its full output is published

An accepted refusal prevents full-manifest commitment and retains a typed refused terminal
under every local policy. Once the full output is accepted, late refusal batches are rejected
without changing that record. Delivery checks the subscriber's actual terminal before an agent
returns success. Acceptance keeps its original attempt separately from a replacement lead's
delivery attempt; equivalent cache pack partitions must contain identical key/kind/byte values.

A completed grant phase is not a completed unit. The unit's current lead keeps heartbeating
while the ordinary `Build` finishes its tail, with the committed grant values ahead of its
store. Helpers and subscribers that arrive during this interval wait for the full record.
A unit with no grant names still has one lead completing its tail.

The lead uploads the full output's immutable packs before `CommitPublication`. That call
atomically checks the lead, attempt and completed root set, and accepts one immutable record
inside the coordinator's sans-IO execution. It is the publication linearization point. The
record survives a lead replacement and cannot be changed by a retry or another lead. The
unit's complete identity covers the inputs of this record; the code generator owns that key.

Cache PUT is delivery of this accepted record, not an authority check. Any subscriber can
mirror exactly the accepted record and acknowledge `DeliverPublication`, which ends the
execution and emits its normal assignment stops. A lost reply or a dead lead after acceptance
therefore needs delivery, never another compile. A stale attempt can upload immutable blobs,
but cannot commit a new record. A terminal outcome is never resurrected by a late delivery.
The coordinator is ephemeral: restarting it changes its epoch and loses live executions;
only the cache mirror is a durable remote result.

The full record contains fresh puts and every successful store read made by the lead's
normal build, with the real value kinds, plus the debug side tables its templates name. A
warm tail must be transferable to a subscriber whose store is empty. Recording keeps only
keys while codegen runs and copies each consumed value once when making the output pack.
This does not add a compiler or a second store.

The rejected ordering was `work done -> Done -> build tail -> publish`. It admitted duplicate
executions while no list was available and let every subscriber compile a separate tail.
A heartbeat followed by an unfenced mutable PUT was also rejected: fencing can change between
those two operations. Attempt-checked acceptance and immutable delivery separate those roles.

Every wait remains bounded by the subscription deadline. Cancellation leaves its subscription
and is terminal under every local policy. Missing or corrupt committed packs, malformed published
manifests, ambiguous publication acknowledgements and failed first-writer verification are also
terminal, including under `LocalPolicy::Fallback`. An unavailable carrier before commitment may
follow that explicit local policy. Real compiler refusals retain the lead's original diagnostics.
A first-writer conflict accepts different pack partitions only after verifying identical logical
keys, kinds and bytes; the existing manifest stays immutable. The cache server's conditional
repair API and durability simulation remain available for explicit repair of absent packs, but
an agent never recompiles or repairs a published absence as a cache-miss recovery path.

The Hegel simulation in `blaze-dist/src/jobs/publication_tests.rs` draws actor steps, virtual
clock advances, early verdicts, failed mirrors, retries and lead replacement. Its oracle is
that every successful execution had one uploaded, accepted and delivered full record and
that acceptance never changes. Its planted early-Done control must violate that oracle.
Carrier tests pause the lead between grant completion and publication, submit another
requester, and verify one execution and one tail; a warm-tail value must reach an empty reader.
