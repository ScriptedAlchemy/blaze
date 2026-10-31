# Distributed compilation and cross-PR reuse review

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


## Original 2026-10-30 review

2026-10-30. Scope: the 24 merged PRs reviewed on the fork's integration line, plus open
[PR #15](https://github.com/ScriptedAlchemy/blaze/pull/15),
[PR #18](https://github.com/ScriptedAlchemy/blaze/pull/18), and
[PR #27](https://github.com/ScriptedAlchemy/blaze/pull/27), including this review's follow-ups.
This is an implementation assessment against the owner's goal: Bazel-like distributed
compilation and caching for Blaze's compiler, with unchanged base code reused across PRs.

| Coverage | PRs | Treatment |
|---|---|---|
| Merged | #1–14, #16–17, #19–26 | Review the integrated behavior; design/research documents are historical evidence, not implementation acceptance. |
| Open source/identity work | #15 | Check portability, resolved features and host/target selection. |
| Open execution/native work | #18 | Check source replay, owned producers, native closure and compiler integration. |
| Open documentation cleanup | #27 | Check the simplified prose against the implementation and its explicit backend boundaries. |

## Outcome

The stack has useful working foundations: persistent compiled-function storage, remote pack
transfer, canonical build descriptions and source replay, compiler batch export/import,
and scheduler state machines with a real local protocol carrier. They are not yet connected
into a production path that splits one Rust build across machines. Passing the scheduler
and D0 exporter tests does not establish that integration or a distributed speedup.

The central distinction is between **reusable code identity** and **one execution's identity**.
Different PR builds should retain their own source descriptions and completion state while
sharing entries whose semantic inputs are unchanged. The existing code supports that direction;
connecting the layers must preserve it.

## What can be reused today

The persistent compiler artifact is a function instance: a non-generic root, a monomorphized
generic function, closure, coroutine, or shim. Its entry contains machine code, relocations,
data, unwind information, and optional debug information. Entries are sealed into packs.
Cargo units describe configuration and dependency relationships; they are not the smallest
compiled artifact. See [cached.rs](../crates/hir-ty-codegen/cached.rs), Entry, and
[stable.rs](../crates/hir-ty-codegen/stable.rs), StableNames::instance.

Persistent invalidation is nevertheless conservative at the crate dependency-closure boundary.
StableNames::spell_crate prefixes owned names with a ContentId. In
[content.rs](../crates/ra-jit/src/content.rs), Contents::hash includes package source bytes,
generated inputs, configuration, environment inputs, and dependencies. Editing one
function changes names throughout its crate and the local crates depending on it. Unchanged
dependencies and unaffected workspace crates remain reusable across PRs. Within an already
running daemon, Salsa can retain unchanged functions of an edited crate; the disk key does
not currently promise that same fine-grained reuse across processes.

For example, two PRs changing different application crates can share an unchanged common
library. A PR changing the common library invalidates its persisted function entries and
dependent crate identities, even when some of those functions' text is unchanged. This is
a safe but broader invalidation boundary, not a correctness defect.

Environment identity also limits reuse. The #15/#18 repair removes exact cache-service
controls from a copy used to key ordinary crates when no transitive procedural macro is
present. Effective values read by `env!` or `option_env!` still affect the identity, including
reads in dependencies, signatures, constants and codegen attributes. Actual source and
producer environments are unchanged. Other declared environment inputs remain conservatively
keyed; for example, per-PR CI variables can still prevent reuse even when source is unchanged.
Consumers of transitive procedural macros retain their represented environment because macro
reads are not observed individually. Complete native-byte macro provenance and a portable
producer-result cache remain separate work. Matching source text alone does not promise a hit.
See [content.rs on #15](https://github.com/ScriptedAlchemy/blaze/blob/dist/c4-keys/crates/ra-jit/src/content.rs),
environment_hash/keyed_environment, and the
[tracked input discovery on #15](https://github.com/ScriptedAlchemy/blaze/blob/dist/c4-keys/vendor/rust-analyzer/crates/hir/src/input_owners.rs).

## Identity boundaries

| Identity | What it establishes | Reuse consequence |
|---|---|---|
| BuildSpec and JobContent | Exact source, resolved units, flags, environment, platform and toolchain; duplicate-job coalescing | Different revisions may be different executions without losing shared code-cache entries. |
| FrontEndKey and ConfigKey | Compatible graph/configuration for warm frontend selection | Omit changing source content deliberately; neither certifies reusable compiled bytes. |
| Stable instance name and Entry::key | Content-qualified instance, generic arguments, overflow policy, target, backend, MIR level and debug policy | Eligible unchanged code can be shared without a PR number or checkout directory in its identity. |
| Store Identity | Store format, compiler build identity and toolchain | Incompatible compiler producers cannot serve each other's entries. |
| ExportContext and receipt | One admitted owned closure, root set, link policy and effective lowering settings | Whole receipts require matching contexts; they are not the universal cross-PR cache key. |

The definitions are in [shapes.rs](../crates/blaze-wire/src/shapes.rs),
[assemble.rs on #18](https://github.com/ScriptedAlchemy/blaze/blob/dist/c2-buildspec/crates/blaze-spec/src/assemble.rs), front_end_key/config_key,
[ra-jit-store](../crates/ra-jit-store/src/lib.rs), Identity, and
[export.rs](../crates/hir-ty-codegen/export.rs), ExportContext::resolved.
The compiler's [build.rs](../crates/ra-jit/build.rs) hashes relative source names and bytes,
features and target, rather than treating every Git commit as a different compiler.

Supported paths are explicitly rebound through the Cargo-home, standard-library and checkout
namespaces; adjacent source paths can also be portable. Arbitrary path-bearing literal data
and unsupported external layouts are not automatically shareable. Proc-macro keys still
retain some environmental/path inputs; simply removing these inputs would be unsound when a
macro can observe them. See
[relocate.rs on #18](https://github.com/ScriptedAlchemy/blaze/blob/dist/c2-buildspec/crates/hir-ty-codegen/relocate.rs)
and [cache.md](cache.md).

Build-script run records and leased proc-macro images are currently local producer caches.
Their descriptions retain execution paths and environmental inputs, and captured execution
creates fresh producer generations. A portable producer-result cache still needs logical
input identities and immutable output bundles; shared compiler packs alone do not provide it.

## Content-addressed data and mutable discovery

The cache separates immutable data from mutable discovery. Sealed packs, key indexes and
unit blobs are verified against their content digests. Per-project manifests and unit lists
are mutable, with conditional publication. This is implemented in
[store.rs](../crates/blaze-serve/src/store.rs), put_blob_from/put_list, and
[remote.rs](../crates/ra-jit/src/remote.rs).

The planned CrateKey-to-CrateRecord-to-PackRef lookup exists in the wire schema, but the
production server does not implement its lookup or publish endpoints. Its
[route](../crates/blaze-serve/src/server.rs) handles hello, missing blobs, object listings,
blobs and lists; hello explicitly has no record/identity table. Current remote reuse therefore
uses project-manifest discovery. Fine-grained crate publication and its execution/GC retention
integration remain work to connect, not functionality demonstrated by schema tests.

## Missing production connections

The gaps are visible in the call graph:

- Production code does not call D0's compile_batch or import_batch; their callers are in
  [export/tests.rs](../crates/hir-ty-codegen/export/tests.rs).
- [Holder](../crates/blaze-dist/src/holder.rs) emits Effect::Compile(Plan), expecting an
  executor to return a sealed, uploaded Compiled result. That compiler adapter is absent.
  The [local CLI](../crates/blaze-dist-local/src/bin/blaze-dist-local.rs) configures no
  workers and logs effects. Its actual retry/fencing protocol is a carrier milestone.
- compile_batch calls compiled_root/compiled_instance before consulting installed entry
  bytes. It exports selected queries; it does not implement the pre-compilation cache lookup
  corresponding to the wire's FromStore result. A hit must also expose dependency coverage.
- LocalRequest retains process-local compiler identities. A worker needs a registry from
  portable work names to retained requests and the owning database. Nonempty caller bounds
  explicitly remain local until a canonical semantic environment key exists.
- import_batch checks producer/context, entries and a closed dependency set. The lead still
  needs a production path that assembles its final object from admitted helper entries,
  compiling only the remaining local work.

C2's saved-source executor and native bindings provide relevant inputs for this bridge; they
do not themselves dispatch function batches remotely. The review fixed the ordinary owned
command's dropped remote-cache service context: an explicitly active RA_JIT_CACHE and configured
RA_JIT_REMOTE/RA_JIT_PROJECT now reach the existing compiler pull/push hooks. The normalized
transport settings are installed after producer preparation; forwarding does not rewrite the
BuildSpec or the source environment already recorded in producer descriptions. Saved replay
retains its isolated service boundary and does not acquire an ambient worker remote.
Default cache-off behavior is unchanged; not every replay participates in shared caching.
The [frontend image](../crates/ra-jit/src/front_image.rs) is an optional prototype over encoded
dependency parts, not a shared live frontend or proof that each worker's frontend cost is gone.

## Repairs from this review

The changes address concrete interactions needed by that architecture:

- **D0:** fingerprint effective lowering policy, reject mismatched provided instances, and
  carry checkout bindings through batch writing and imported Entries. Tests exercise real
  compiler queries, receipt import and consumer-side path rebinding.
- **Scheduler:** expire deadlines before submission coalescing and capacity decisions, so a
  new subscriber cannot revive expired execution state. See Jobs::submit and Coordinator.
- **HTTP cache:** release listing snapshots after oversized-page failures and avoid consuming
  slots for completed single-page listings; reject malformed manifest-path encodings.
- **Owned cache configuration:** preserve explicit shared-cache, project, trust, transport and
  resource policy for ordinary compiler children. Caller-relative service paths are resolved
  before child directories change. See
  [cache_service.rs on #18](https://github.com/ScriptedAlchemy/blaze/blob/dist/c2-buildspec/crates/ra-jit/src/cargo/cache_service.rs).
- **Compiler environment identity on #15/#18:** discover inputs before memoizing identity; key effective
  observed values, including authoritative crate overlays and dependency reads; distinguish
  unset, empty and non-Unicode values; and exclude only unobserved cache-service controls for
  ordinary crates. Lazy symbol/section attributes use the compiler's existing semantic
  queries. This fixes both unnecessary cache misses and stale reuse of a library whose
  embedded environment value changed. Proc-macro consumers retain conservative text inputs.
- **T1 tests:** identify executables and recorded inputs by content/mode, retain symlink text
  and target observations, and revoke old passes after forced or filtered failures. See
  [affected.rs](../crates/ra-jit/src/cargo/affected.rs),
  [sandboxed.rs](../crates/ra-jit/src/cargo/sandboxed.rs), and
  [run.rs](../crates/ra-jit/src/cargo/run.rs), test_with. The #18 follow-up shares this policy
  with replay through cargo/spec/test_policy.rs::Policy and execute_inner, including retry,
  opt-out and pass-revocation behavior. cargo/spec.rs::admit_trace preserves native filenames.
- **Owned producers and resolution:** preserve declared environment/removal boundaries and
  stable tool identity; resolve dependency required-features and host/target feature domains
  consistently. These feed the compiler's input identity, rather than being independent
  metadata details. See cargo/spec.rs, cargo/sandboxed.rs and blaze-metadata's programs APIs.

## Smallest next production bridge and acceptance

Connect the existing source replay, Holder and D0 APIs in one bounded path before expanding
the scheduler. Start with two independent worker processes through the local carrier, then
exercise the same adapter over a multi-host transport. Acceptance should establish:

1. Both workers materialize the authenticated BuildSpec, agree on canonical roots, and retain
   actual local requests. Each has a separate local store, and both receive and compile real
   work from one build.
2. Cache hits are admitted before compile queries. Misses run through D0, upload packs, and
   commit under the scheduler's lease; retries cannot duplicate accepted accounting.
3. The lead imports the accepted closure and links/runs the program. Counters demonstrate
   that accepted helper work is not compiled again during final assembly.
4. A second PR, in a different checkout root and fresh process with an initially empty local
   store, changes only another application crate. Its only reusable artifacts come from the
   first build's remote. With compatible compiler, settings and declared inputs, unchanged
   shared-library entries are retrieved remotely with zero backend compilation of those
   eligible instances. Changed inputs produce the expected misses.
5. Worker loss and a missing/corrupt pack take the existing retry/local path without claiming
   nonexistent completion. The final behavior agrees with a fully local Blaze build.

Record compiler-query counts, hits/misses, local remainder, transferred bytes, frontend time,
peak memory and end-to-end elapsed time. These establish correctness and explain costs;
speedup requires a separate measured comparison on representative workloads.

Environment discovery reuses the tracked owner walk already needed for external source
identity. Completing local identity also visits lazy signature/constant owners and codegen
attributes. Results are memoized per crate; this is a conservative correctness cost, not a
measured frontend optimization. Compiled-code cache hits still require frontend work.

## Evidence limits at writing

Cache/scheduler changes passed 262 crate tests; the simulation additionally passed with
10,000 seeds each for job and work scenarios. The earlier D0 default-Cranelift suite passed
50 tests, with its process-child fixture invoked by parent tests. Combined compiler
validation subsequently passed 101 focused tests, and the full combined codegen suite passed
67 tests, with one subprocess helper exercised by parent tests. After the final environment
repair, 13 Content tests passed through the real Resolver and frontend database, and the
full default-Cranelift codegen suite again passed 67 tests with its one subprocess helper
exercised by parent tests. The initial identity and cold-attribute cases were reproduced
failing before their fixes. The direct service
handoff passed six tests, including publish from one process followed by lazy retrieval into
another process's fresh local store and a read-only peer check. Metadata suites passed 21 tests
on C4 and 34 on C2, including the Cargo unit-graph oracle. These are separate branch receipts,
not an aggregate count of unique tests.

The saved-execution harness now includes the shared test-policy and two-build remote-cache
cases. Their full owned compiler/runtime execution remains a CI acceptance gate; a harness
self-check or a transport round-trip does not establish that runtime result. Component and
integration regressions are not a two-host build receipt. No LLVM/macOS or distributed
performance result is claimed here.
