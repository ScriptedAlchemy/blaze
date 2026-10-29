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

Next: schedule nested producers topologically with leases and unchanged image
reuse. Close script parity gaps before switching both cold paths. Never derive
a plan from rustc artifacts.

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
bootstrap still expands its own derives through the existing compile-time-deps
handoff. `bootstrap-audit.json` retains every compiler/Cargo call, without
excluding compile lines. This bootstrap is not a strict-gate pass.

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

No cold path was switched. The strict initial seven-row witness remains FAIL.

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
to `answers.rs`'s owned answers, never rustc. Compile probes remain
unfinished as a general compiler interface. The frontend can report some
source errors, including cfg-expanded channel gates, but cannot certify
compiler acceptance: stability, privacy, const validity, dyn compatibility
and unresolved signatures are not fully checked. An apparently valid probe
through that partial checker therefore refuses loudly, as an undecided
operation. It publishes no successful compile probe until those checks are complete. Unknown artifact
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
it does not establish the rest of the proc-macro server bootstrap or broaden
the partial checker's acceptance.

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
The P8 review's 22 cases now require oracle parity or a loud undecided refusal.
The default registry compiler-probe closure currently refuses; its earlier
success is not current parity evidence. `producer_probes.py` also checks
known errors and explicit undecided/bootstrap refusals. This is not a general
metadata-emission or feature-catalog parity claim. The current default cold
route is documented in [cargo.md](../cargo.md); neither that routing change
nor the empty-library proof establishes full compiler acceptance or a passing
strict native-server bootstrap gate.
