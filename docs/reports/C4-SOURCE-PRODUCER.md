# C4 source producer boundary

This is an independently invocable compiler boundary, not a cold-path switch.
`ra-jit proc-macro-source <sysroot> <host-plan.json> <exclusive-output-dir>`
consumes a JSON rendering of `Vec<own_script::Planned>`. The first unit is the
proc-macro root. Every dependency is a source root with its own edition, cfg,
codegen flags, environment and renamed edges. No supplied dylib or wrapper
record is accepted. Empty graphs, relative source roots, cycles and out-of-range
edges fail before starting the compiler.

The boundary runs blaze itself with `--shared`, the implicit `proc_macro`
source dependency and complete dep-info. Standard library resolution and query
answers are forced to be owned, including through the `BLAZE_` aliases.
An unanswered query fails instead of spawning rustc. A store can supply std
previously compiled by this blaze; this boundary does not acquire a shipped
std artifact. The output directory is exclusively owned by the caller until
the child ends. Publication leases and before/after validation are not added
by this boundary.

## The application-derived host plan

`ra-jit proc-macro-plan <application-manifest> <selection.json> <host> <target>`
renders the source units derived by blaze's own resolver and profile planner.
`proc-macro-build` takes the same arguments followed by an exclusive output
directory and builds the reached source producers. It does not consume the
rendering, a wrapper record or an oracle artifact.

`blaze-metadata::build_units` walks the application's `ResolvedFeatures`
before metadata folds host and target features into package nodes.
`ResolvedUnit::host` identifies `FeaturesFor::HostDep`, not whether a triple
happens to equal the native host. The one target compile kind comes from the
application selection; proc-macro and build-dependency edges select Host.
Normal dependencies retain their parent's feature domain. Optional activation
is read in that domain, target predicates use its platform facts, and extern
names come from the owned resolver. Host-only packages omitted by the folded
target metadata are retained from the same resolution, not resolved again.
Resolver 1's intentional feature union is preserved.

`cargo::producer_plan` uses owned target cfg answers, owned profiles and
`Config::extra_args`. Explicit `--target`, even when it names the native
triple, keeps target rustflags away from host producers. Feature cfgs,
debug assertions, overflow checks, panic, additional cfgs, package environment
and config environment are per unit. Host profiles use build overrides.
Source closure reindexing marks only the producer root; the compiler adds
the implicit source `proc_macro` dependency.

The diagnostic renders library or selected package roots, not all target
kinds of a complete Cargo unit graph. The first executable witness selects
one binary application and library producers. Build-script units are compiled
and run through the owned runner, with their host feature domain and generated
cfg, environment and OUT_DIR. `proc-macro-build` still fails before any compiler
child if a producer closure needs a nested proc macro.
This is an unfinished scheduler boundary, not a restriction on the intended
product. No cold path calls it yet.

`harness/dist/producer_plan.py` compares native, explicit-native and cross
plans with an independent Cargo unit graph. It checks disjoint features,
optional renamed edges, host predicates, profile overrides, cfg and config
environment. It compiles the producer from the application's manifests
through `proc-macro-build`, expands with the matching owned server, and
compares with a separate Cargo-built reference. Cargo receives the manifests,
not the product's serialized graph. The product tool audit retains queries
as well as compile calls. A Rust regression compares unit features directly
with the owned resolver's result under resolver 1, 2 and 3.

The captured producer API in `scripts::captured` schedules authoritative source
units, script compilations/runs and nested macros topologically. It retains
producer generation leases through the caller's compilation, records source,
runtime and generated inputs separately, and uses immutable macro images with
before/after snapshots. The caller supplies the resolved graph; this API does
not rediscover it through Cargo. This is separate from the diagnostic
`proc-macro-build` adapter's remaining nested-producer restriction.

Captured descriptions use compiler mode, explicit per-unit cfgs and target
kinds. Native environments and flags retain OS bytes and per-description unit
IDs, including distinct instances sharing a source file. The private transport
goes over stdin; the public description redacts environment values. Producer
image records also retain native source/output/alias path bytes. These data and
consumer tests do not establish raw-filename source topology: the shared
renderer currently aliases a raw filename without its sibling directory.
Actual native-source runtime acceptance remains separate.

## Separate compiler witnesses

`harness/dist/source_macro.py --light` supplies a two-unit resolved graph:
a derive/function-like producer and a path dependency with `host` cfg.
The registry witness supplies actual syn, quote, proc-macro2 and unicode-ident
sources, using their non-proc-macro parser/token representation. It uses
string conversion at the proc_macro boundary. It deliberately does not run
their default-feature compiler-probing build scripts. Neither supplied graph
proves that the application planner derives host features correctly.

The harness's separate rustc oracle builds its own rlibs and dylib, expands
both macros through the reference server, and compiles/runs the target-only
shared dependency. The full witness needs `--owned-server` built by blaze with
the matching proc_macro bridge. It compares normalized token structure/text;
server-local span ids are ignored. It does not claim span parity.
`--build-only` explicitly does not pass that expansion gate.

`harness/dist/owned_macro_server.py --blaze <blaze> --work <server-build>`
builds the matching server executable through blaze, including its source
`proc_macro`. It does not substitute the toolchain's server. The server's
protocol uses explicit Serde implementations rather than production protocol
derives, with independent JSON/postcard wire comparisons. Transitive upstream
packages still declare unused proc-macro dependencies; the production dependency
graph is not described as derive-free. Bootstrap disables the external macro
server and blocks compiler/Cargo tools and the official toolchain server.
`bootstrap-audit.json` and the absolute-exec census retain every attempted call,
without excluding compile lines.

On Linux x86_64, source head `42abdbad3b39ff682b20ae16dd745718058706b5`
passed a fresh bootstrap in 611.024 seconds with its own target, home, store and
work directory. Every linked image reported zero uncompiled functions; the new
server's version check passed. Both audits were empty. Server SHA256 was
`8e3237af6e96fd16ae18baaecce16a2ccfeb56b513f859e52853511b391ba8ed`.
This receipt precedes native-context integration and does not certify later
combined heads.

The full source-producer witness audits both producer compilation and owned
server loading/expansion. It retains the actual owned request/response
transcript, normalized comparisons and SHA256 pins of blaze, server, producer
and independent oracle images. Run it twice, with `--light` and without, over
fresh work directories. A matching token result does not prove host planning,
default registry features, build scripts or span parity.

On this Mac, Python injects `SDKROOT`. Release, cfg, filename and target-spec
queries do not read that linker input. Owned answers preserve it rather than
clearing the environment; 63 SDK/target differential controls match Rust 1.99.0.
Deployment-target override modelling remains incomplete and fails by name.

The default owned cold route is documented in [cargo.md](../cargo.md). The
original strict seven-row witness failed at the earlier reviewed head; a fresh,
unexcluded seven-row witness and plain token scan are still required on the
final combined head. The isolated bootstrap receipt does not establish them.

## Owned producer scripts

`proc-macro-build` projects the application's resolved host units into the
existing `scripts.rs` runner. The adapter replaces folded metadata features
and edges with host-domain features and renamed normal/build edges. It gives
the runner the owned host flags and cfg answers. The runner compiles and runs
the scripts itself, with its existing freshness records, admission and output
generation leases. The command keeps those leases and pins through producer
compilation. Generated cfg, env and OUT_DIR reach each host library; the link
reads native inputs from the script output beside OUT_DIR as before.

Scripts name an owned probe launcher. Version and print questions go straight
to `answers.rs`'s owned answers, never rustc. Compile probes are
validated through a tracked vendor compiler-acceptance query covering active
signatures, resolved API stability/privacy, dyn compatibility, constant rules
and borrow checking, combined with existing inference diagnostics. Unsupported
validation remains an explicit producer error. At head `42abdbad`, all 22 review
probes matched Rust 1.99, including eight positive cases and a positive script
E2E witness. A loud undecided refusal does not pass this parity gate. Unknown artifact
operations and bootstrap feature attributes also record a failure
which the runner checks before publishing successful script output. A script
cannot swallow an unsupported operation and silently select incomplete cfgs.

An empty library translation unit now has a separate complete proof in
`syntax::empty_crate`: every syntax node and token must be accounted for, with
no items, recovery, conditional attributes or unchecked crate attributes. An
exact inner `no_std` attribute selects core; ordinary empty source selects
std. Both require the corresponding source libraries. The owned compiler
validates the crate name, edition, target and emission request, then atomically
writes a genuine `.ll` module with the owned target layout and triple,
including macOS deployment-version clamping. Unknown codegen options and
other requested artifacts remain unsupported. No compiler or Cargo process
implements this case.

On 2026-10-29, the real `autocfg` 1.5.1 constructor and the cached `memoffset`
0.6.5 and 0.9.1 build scripts produced identical version decisions and cargo
cfg output through the owned launcher and a separate Rust 1.99 oracle. The
product's compiler tools were blocked and counted: zero calls, no unsupported
marker or swallowed probe warning. The `autocfg` constructor consumed its
emitted LLVM files normally. This closes the empty-source constructor failure;
that constructor receipt alone does not establish server bootstrap. The later
isolated bootstrap and probe receipts above are separate checks.

`harness/dist/producer_scripts.py` uses the vendored token producer with a
default feature and build script. It compares expansion with an independent
Cargo/rustc oracle. The producer uses a renamed build dependency, host-only
metadata from a dependency script with `links`, generated include source,
cfg and env, and an owned version query. A repeat checks that script executables
and outputs stay unchanged. A swallowed compile probe is a negative control.
Every product tool call, including queries and expansion, remains audited.

`harness/dist/producer_registry.py` builds the existing source producer with
real registry syn, quote and proc-macro2 default features and scripts. Script
stdout and normalized expansion matched the independent Cargo/rustc oracle
at 623e738b. That partial checker failed open on broader compiler probes.
The 22-probe review gate now requires exact oracle parity without undecided
refusals; it passed at `42abdbad`. The default registry source/script/expansion
gate still needs an actual final combined-head run. Earlier output parity is
not that receipt. Neither a focused semantic test nor an empty-library proof
substitutes for the registry, fresh-server, seven-row or plain token gates.
