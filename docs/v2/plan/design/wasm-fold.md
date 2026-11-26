---
status: plan
date: 2026-11-26
summary: Portable wasm constant folding before sandboxed Wasmtime compilation.
---
# Portable wasm folding

Equivalent straight-line numeric bodies share one appended function, taking varying typed
constants as extra parameters. Each original index retains a thunk with its original signature
and local declarations. Original calls, exports, tables, start and name indices remain valid.
Template local indices shift past the appended parameters. Unknown operators are ineligible,
not interpreted by an incomplete decoder; unsupported offset-bearing metadata is refused.
The input and assembled output both pass the runtime's pinned wasm validator.

Groups are ordered by their first original function index, with members in function order and
parameters in instruction order. Keys cover the original module, transformation version and
membership. Each group has one template task and a distinct task for each member thunk; their
names also cover the role and original/appended function index. A grant renders only its named
bodies. The existing agent uploads their immutable packs and fences the accepted result by its
lease. Assembly requires every expected record exactly once; a missing holder never causes an
ungranted local completion. Only these actual output keys are marked as folded emissions, and
the acceptance oracle must join them to accepted grants of the same execution and attempt.

The runtime's preparation identity joins its compiled-code key. On a sealed AOT hit no wasm is
read and no preparation or distributed work occurs. On a miss, the agent can read an existing
published portable unit without emitting work; the assembled module still passes the full
validator before Wasmtime compiles and runs it inside the existing sandbox. Original bootstrap
module identities remain unchanged. Wasmtime's opaque native compiler objects are never
serialized as distributed records. `.rustc`, names, producers and target features are preserved;
unknown custom sections, including code-offset debug metadata, are refused when folding.

The pure transformation's execution oracle checks original and transformed exports against
independently calculated constants, including local-index remapping. Record-set controls remove
and duplicate outputs and attempt to reuse them after editing constants. Runtime controls
remove the wasm after cold preparation and require zero callback calls on the warm load.
Real two-holder publication, stale-lease refusal and the proc-macro invocation remain acceptance
requirements; these are not established by the pure transform. The floor is one input decode and one output write;
group matching sorts normalized bodies rather than comparing every body to every other body.
