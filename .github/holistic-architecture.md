## 2026-10-31: whole-fork integration follow-up

This update uses the 2026-10-31T22:26:53Z snapshot of all fork branches, all 34 retained
PR heads, and the parent repository's branch ancestry. The inventory contains 844
fork-only commits (757 non-merges and 87 merges). It is a history and integration
review, not a claim that every historical line has an independent execution proof.
There were four open PRs: #18, #32, #33, and the deliberately frozen, do-not-merge
macOS evidence PR #34. #15 had landed in `dist/main` by this snapshot. The older
review below retains its original source scope.

### The change in emphasis

Keep the existing compiler, store, admission rules, and scheduler state machines.
The largest missing capability is their **production connection**, not a new
scheduler, a replacement wire format, or an additional abstraction layer.
`compile_batch` and `import_batch` still have no production caller, and the local
protocol carrier is not a worker that compiles assigned function batches. Thus the
fork still cannot claim that one real build is split between two workers merely
because the scheduler, replay CLI, cache, and D0 tests independently pass.

The [owner's parity scope](distributed.md#scope-parity-with-bazel-and-nativelink)
already requires this connection. Do not broaden it into kernel sandboxing,
capability/signature systems, a new frontend image format, or whole-crate action
execution under a different name.

### Corrections included in this review stack

* Treat the remote manifest watermark as **successfully installed read state**.
  Publishing local packs cannot acknowledge another worker's concurrent entries.
  Only a completed pull/lookup installation advances it. A successful upload is
  not proof that newly listed packs are locally available.
* A failed lazy pack fetch is a failed attempt, not a permanent fact about a pack.
  Retry through the existing throttled keeper lookup; do not turn every compiler
  cache miss into a new network attempt or discard a working table on an index
  refresh failure.
* The compiler's source identity must describe a complete source walk. Unreadable
  sources, broken aliases, cycles, and unrepresentable names select the existing
  executable-byte identity fallback; they do not create a portable-looking digest
  of a partial compiler. Hash directory alias contents and preserve real filename
  characters while retaining checkout-location independence.
* Reconcile C2's native ancestor-link and workspace-doctest changes with #32,
  preserving the build-once consumers. Integrate #33's verdict corrections and the
  already merged Windows macro environment fix rather than duplicating them.
* Keep the same benchmark alignment settings on both producer builds and record
  their effective Rust flags. Consumers compare provenance without recompiling.
  Preserve the stricter existing comparison/control verdict, threshold and noise
  policy; a failed control is not evidence of harmless noise.
* Test PRs whose base is another branch in the stack. Cancel obsolete PR runs, but
  retain separate full acceptance runs for integration pushes.

### The next production slice: one existing path, end to end

Use the existing owned build/replay path as the entry point. The local and remote
executor should share the same admitted BuildSpec, producer preparation, compiler
request registry, and result admission, differing only in transport and placement.
Do not bolt a second independent compile pipeline onto `blaze-dist-local`.

1. **Request registry and lifetime.** Bind stable `WorkName` values to local
   `LocalRequest`s and the database that owns them. Resolve portable names against
   the worker's admitted source/producer closure. Never transmit Salsa or interner
   IDs. Keep requests with unsupported caller-bound identity explicitly local.
2. **Cache before backend queries.** Ask for admitted reusable entries before
   calling `compiled_root`/`compiled_instance`. A hit needs its covered dependencies,
   not just a blob named by a key. A whole-build receipt/context is an admission
   boundary, not the universal cross-PR cache key. Retain existing function-entry
   keys and content-qualified dependency identities.
3. **Execute an assigned miss.** Connect `Holder::Effect::Compile` to the owned
   batch exporter, report discovered callees, seal/upload output, and return the
   existing terminal result under the coordinator's lease/generation authority.
   Losing a worker may retry owned work locally; it must not invoke Cargo/rustc as
   a compilation fallback or accept a stale helper's completion.
4. **Admit and assemble.** Collect a closed accepted entry set. Assemble with a
   fresh `Entries` generation so an earlier negative lookup does not hide received
   entries. Measure that assembly does not silently recompile the assigned work.
5. **Reuse across a second PR.** Change application source but not a shared
   dependency, start from a new checkout and empty local store, and prove reuse of
   the unchanged dependency before scheduling backend compilation for it.

The first required witness uses two real worker processes, distinct stores and an
owned Rust fixture with a dependency, a generic call, and final native linking.
It records stable work names, lookup outcomes, backend query counts, lease/fencing
results and accepted entry keys. A transport echo, a fake compiler, one whole job
moved to a remote machine, or two processes sharing a local target directory is
not that witness. A local two-process test precedes the two-host measurement.

### Completion criteria and order

| Boundary | Required evidence |
|---|---|
| Cache hit | Fresh local store imports admitted unchanged dependency code; its backend query count is zero. |
| Real parallel work | Two workers complete disjoint batches of the same build; no duplicate assembly compilation. |
| Cross-PR reuse | Application edit changes its identity without preventing unchanged dependency reuse. |
| Failure handling | Lost lease, failed upload, corrupt/incomplete entry set, and unavailable coordinator preserve correctness through existing retry/local-owned behavior. |
| Native/runtime correctness | Final program and tests match the existing Cargo reference; compiler-spawn audit remains empty for product phases. |
| Acceptance | Full C2, saved Cranelift/LLVM, and platform checks finish on a fixed integration head; frozen #34 remains an independent macOS diagnosis. |

Land the proven correctness repairs, reconcile the stack, then build this single
vertical slice. Defer finer-grained persistent invalidation and frontend-memory
rewrites until the slice measures reuse and duplicated frontend cost. The current
crate-closure invalidation is conservative, not permission to remove inputs from
keys. A future portable producer cache and remote test-result publication must
use the same ownership/lifetime boundaries rather than add parallel schedulers.
