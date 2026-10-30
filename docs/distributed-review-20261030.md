# Distributed compilation and cross-PR reuse review

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
generated inputs, configuration, observed environment values, and dependencies. Editing one
function changes names throughout its crate and the local crates depending on it. Unchanged
dependencies and unaffected workspace crates remain reusable across PRs. Within an already
running daemon, Salsa can retain unchanged functions of an edited crate; the disk key does
not currently promise that same fine-grained reuse across processes.

For example, two PRs changing different application crates can share an unchanged common
library. A PR changing the common library invalidates its persisted function entries and
dependent crate identities, even when some of those functions' text is unchanged. This is
a safe but broader invalidation boundary, not a correctness defect.

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
do not themselves dispatch function batches remotely. At review time, the saved executor's
default cache-off behavior and handling of explicitly configured remote-cache settings are
under active review. Do not claim that every replay already participates in shared caching.
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
   actual local requests. Both receive and compile real work from one build.
2. Cache hits are admitted before compile queries. Misses run through D0, upload packs, and
   commit under the scheduler's lease; retries cannot duplicate accepted accounting.
3. The lead imports the accepted closure and links/runs the program. Counters demonstrate
   that accepted helper work is not compiled again during final assembly.
4. A second PR, in a different checkout root, changes only another application crate. With
   the same compiler and settings, unchanged shared-library entries hit the cache with zero
   backend compilation of those eligible instances. Changed inputs produce the expected misses.
5. Worker loss and a missing/corrupt pack take the existing retry/local path without claiming
   nonexistent completion. The final behavior agrees with a fully local Blaze build.

Record compiler-query counts, hits/misses, local remainder, transferred bytes, frontend time,
peak memory and end-to-end elapsed time. These establish correctness and explain costs;
speedup requires a separate measured comparison on representative workloads.

## Evidence limits at writing

Cache/scheduler changes passed 262 crate tests; the simulation additionally passed with
10,000 seeds each for job and work scenarios. The earlier D0 default-Cranelift suite passed
50 tests, with its process-child fixture invoked by parent tests. Combined compiler
validation subsequently passed 101 focused tests, and the full combined codegen suite passed
67 tests, with one subprocess helper exercised by parent tests. Metadata suites passed 21 tests
on C4 and 34 on C2, including the Cargo unit-graph oracle; these are branch receipts, not an
aggregate count of unique tests. These are component and integration regressions, not a two-host
build receipt. No LLVM/macOS or distributed performance result is claimed here.
