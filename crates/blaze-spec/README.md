# Build specs

`blaze-spec` reads a build request using `blaze-manifest` and `blaze-metadata`, not a
subprocess running cargo. It returns a canonical `BuildSpec`, the native platform it
names, and the front-end, configuration and job-content keys. Unsupported inputs produce
a named refusal before resolution or scheduling.

The request parser's `FLAGS`, configuration `KEYS`, environment lists and refusal
`CATALOGUE` document which settings are represented, irrelevant or unsupported.
Each unit carries its own resolved profile, including package and build overrides,
blaze's backend, LLVM passes and instruction selection, and MIR optimisation.

The native platform probe resolves C/C++ compilers, archivers and linker drivers and
hashes their executable bytes. Its `native_root` is currently a **proxy**, not a complete
SDK or sysroot closure: see `src/platform.rs` for the files covered and the unverified
Linux cases. Do not claim that two arbitrary native input closures are distinguished.

## CLI

```
ra-jit cargo spec [--unit-graph] [--out FILE] build|check|run|test [cargo flags]
ra-jit cargo spec --run FILE
```

The first command prints a JSON rendering for people, or cargo-shaped unit graph JSON.
`--out` writes canonical CBOR. JSON is not an execution format or an identity input.
Schema 3 keys the selected workspace root manifest relative to the materialized
source root. `cwd` remains workspace-relative. Replay locates that manifest first;
sibling workspaces cannot alias merely because their package metadata matches.
Both paths are escaped byte components, preserving legal Unix colon/backslash
names and non-UTF-8 bytes. Generation/decoding use the same validated type; native
materialization decodes components rather than joining the escaped text. Git API
callers must supply the actual materialized repository root.
Native probing receives OsString values, including configured PATH and custom
compiler wrappers, without substituting ambient variables. Logical path decoding
uses the message budget rather than the ordinary string budget. Git path stdout
is read as native bytes with only its line terminator removed.
Schema 4 gives free-form env/flag values message-sized typed budgets and preserves
raw Unix env values as tagged bytes. Ordinary identifiers retain their string limits.
Native probe methods are required; pathname-query stdout is bytes, not version text.
Flag refusals print `refused.what` and `refused.why` and return status 2.
The CLI reads blaze's own answer table and installed toolchain records for host facts.
It does not execute rustc to discover them.

**Incomplete:** `--run` validates reproduction candidates against the current checkout.
It reports a mismatch with status 2. A matching spec currently fails with a message
that the owned unit executor is not connected. It never dispatches to the existing
cargo/rustc fallback. Connecting an executor for libraries, tests, build scripts, proc
macros and doctests, and passing the real local-run harness, remain C2 acceptance work.
Config values now preserve literal/workspace-relative tags in CBOR. Original
CLI configuration/profile overrides still need complete reconstruction without guessing.

## Tests

```
cargo +1.99.0 test -p blaze-wire -p blaze-spec -p blaze-metadata
python3 -I harness/dist/buildspec.py --bin /path/to/ra-jit
```

Run builds and the real harness through the project's heavy lock and shared target.
The harness has its own memory watchdog. `--no-local` is useful to isolate generation
and unit-graph failures, but is not the acceptance gate.

`blaze-metadata/tests/units` has recorded cargo oracle graphs for eight workspaces.
Library tests do not invoke cargo or rustc. `units_expected` is the explicit oracle
regenerator; its cargo calls are test tooling, not a product fallback.
