---
status: plan
date: 2026-11-12
summary: Give sandboxed build scripts reproducible randomness and an explicit wall clock without accepting untraced inputs.
---
# Build-script providers

A native build script can request randomness before main: glibc allocates while Rust sets up
its stack-overflow handler. The sandbox records that call correctly, but a source purity proof
cannot account for the runtime's ambient input. Removing allocations in blaze's launcher alone
does not remove this read.

Port upstream's provider through blaze's std, blaze-usercode and the Linux sandbox. A run's
seed comes from its code and given-input identities. One allocation-free derivation serves std
and the supervisor. The supervisor supplies deterministic bytes for intercepted randomness;
only calls it actually answers have answered trace records. Unanswered calls retain their
refusal. The wall clock is fixed only when SOURCE_DATE_EPOCH is an input. Ordinary processes
without a provider retain the pinned nightly's behavior. Explicit pure functions still refuse
randomness and the clock. No sandbox permission is widened.

The launcher prepares exec vectors before installing its filter and reports exec errors from
stack storage. A rejected pure run returns an error before either cache can retain it. Loader
probes are recognized only against the environment of that actual run; loaded inputs and
unrelated paths remain checked.

Bounds: reuse the upstream per-call byte, device-open and device-byte limits; one stream per
process and per device open. The seed introduces no new ambient input. Provider derivation
changes invalidate runs through code identity; the library digest is regenerated. Existing
unprovided trace records remain conservative.

Proof: replay the captured parking_lot_core executable under syscall tracing; compare repeated
provided runs and an unprovided control. Hegel checks interleaved independent process streams
against a stateless byte oracle, with a shared-counter plant. Sandbox tests check actual answers
and the pre-exec window, with a planted random call. Trace controls retain unanswered randomness.
A simulated rejected run must miss on retries and after store restart; a planted acceptance must
violate that oracle. Runtime execution of these controls and the original allocator/EH/library/
native/distributed gates is required before acceptance.
