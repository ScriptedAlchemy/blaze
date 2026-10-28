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

## Where the real host plan must come from

The next planner unit belongs in `crates/ra-jit`, over `blaze-metadata` and
`plan.rs`. It must expose resolved compile units rather than metadata's folded
package nodes. `blaze-metadata/src/graph.rs::of_build` currently unions host and
target features into a package node. Those nodes cannot produce this host
graph: `scripts.rs::Runner::described` also reads that union today.

Preserve `(PackageId, FeaturesFor, CompileKind)` while walking the owned
`ResolvedFeatures`. A proc-macro edge selects `HostDep` and the host triple;
normal dependencies of that unit retain the host feature domain, including
optional dependency activation. Evaluate target predicates for that domain.
Use the resolver's extern-name mapping for renamed dependencies. Use the host
profile and effective host rustflags from the owned plan, not target flags.
Add the implicit source `proc_macro` dependency. Execute dependency build
scripts topologically through the owned script compiler/runner before filling
each unit's cfg, environment and native-link inputs. Nested proc macros need
the same host scheduling. Never recover this graph from rustc records.

Test that graph independently against Cargo's unit graph, emitted by the
Python harness only, especially a shared dependency with disjoint host/target
features. Then feed the verified graph to this compiler boundary.

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

On this Mac, Python injects `SDKROOT`. The owned query model currently refuses
SDK overrides. `--default-sdk` selects the default SDK witness and records
that choice; product code does not erase the user's SDK. Closing the SDK query
gap is still required, rather than waiving the audit.

No cold path was switched. The strict initial seven-row witness remains FAIL.
