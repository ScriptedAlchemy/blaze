---
status: plan
summary: Keep provider seed construction outside the sandbox state machine and exclude zero-sized phantom signatures from callable parameter state.
date: 2026-11-12
---
# Provider setup and phantom effect state

A Blaze check of blaze-cargo stops before tests on two different effect boundaries.
Provider seed construction reaches BLAKE3 CPU discovery and thread-local scratch;
these are setup work, while the sandbox randomness state machine must remain sans IO.
Move Answers and its unchanged seed-to-stream transitions into a marked sans-IO
module, retaining the public provider API and existing stream/interleaving oracles.
Provider construction remains at the edge and injects its finished seed into Answers.
No hash, seed format, clock policy or sandbox answer changes. Keeping setup inside
the core with a lint exception was rejected; no effect-lint level is relaxed.

The parameter scan also reaches PhantomData<fn() -> View> inside the daemon's state.
PhantomData stores no callable value and invokes no destructor for its type parameter.
Stop its parameter scan; retain the existing recursive scan for Unique's owned data.
An actual function pointer beside a phantom marker remains refused. Prove both with
one reduced fixture, then run the existing effect-check tests and Blaze cargo check.
The body walk still checks calls and drop effects. No persisted data changes.

Borrow the release error and distributed unit label in the two helpers that only read
them, updating all callers. Existing release and distributed acceptance tests retain
their guards. Validate the exact combined source after the current frozen linker run
settles; do not mutate that run's snapshot or claim unexecuted tests pass.
