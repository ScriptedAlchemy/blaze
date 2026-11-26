---
status: plan
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
membership. Grants must publish actual portable group bytes before recording an emission;
Wasmtime still compiles and runs the assembled module inside the existing sandbox. Its opaque
native compiler objects are not serialized as distributed records.

The first piece is the pure transformation and its execution oracle: original and transformed
exports execute against independently calculated constants, including local-index remapping.
Distributed grant publication and the real proc-macro fixture are required subsequent checks,
not established by the pure transform. The floor is one input decode and one output write;
group matching sorts normalized bodies rather than comparing every body to every other body.
