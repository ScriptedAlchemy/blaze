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
compiler wrappers, without substituting ambient variables. Relative native inputs and
empty PATH entries resolve from the request's saved cwd; probe children run there without
changing the serving process's cwd or rewriting serialized environment values. Logical path
decoding uses the message budget rather than the ordinary string budget. Git path stdout
is read as native bytes with only its line terminator removed.
Schema 4 gives free-form env/flag values message-sized typed budgets and preserves
raw Unix env values as tagged bytes. Ordinary identifiers retain their string limits.
Native probe methods are required; pathname-query stdout is bytes, not version text.
Flag refusals print `refused.what` and `refused.why` and return status 2.
The CLI reads blaze's own answer table and installed toolchain records for host facts.
It does not execute rustc to discover them.

**Saved specs.** `--out FILE` writes the spec to `FILE` and keeps the source it was made from in
`FILE.inputs/`: BLAKE3-addressed blobs of the workspace, its path dependencies, the locked registry
and Git packages, and the configuration files resolution read, plus a replay record (the original
words, the environment they ran in, the configuration layers in precedence order). `--run FILE`
restores that source into a directory of its own, resolves it offline from exactly the captured
configuration (nothing above the directory and nothing in the worker's Cargo home is read), checks
that the result is the saved spec, and compiles and runs it with the owned compiler. The checkout it
is run from, and whether the original still exists, make no difference. It never starts cargo or
rustc. A Cargo home that is neither inside nor around the workspace is kept in the snapshot under
`.blaze-cargo-home/`, so the layout root, and with it the spec's paths and digests, is the
workspace's and not an absolute prefix or the Cargo home's place beside it. Tested: Mac and Linux.

**Not run yet.** Libraries as the asked artifact, `check`, doctests, benchmarks, build scripts,
proc macros and application native link inputs are refused by name, before any compile. Absolute
path dependencies and absolute symlinks are not portable and are refused at capture.

## Tests

```
cargo +1.99.0 test -p blaze-wire -p blaze-spec -p blaze-metadata
python3 -I harness/dist/saved_spec.py --blaze /path/to/ra-jit --work /tmp/saved-spec
python3 -I harness/dist/buildspec.py --bin /path/to/ra-jit
```

Run builds and the real harness through the project's heavy lock and shared target.
The harness has its own memory watchdog. `--no-local` is useful to isolate generation
and unit-graph failures, but is not the acceptance gate.

`blaze-metadata/tests/units` has recorded cargo oracle graphs for eight workspaces.
Library tests do not invoke cargo or rustc. `units_expected` is the explicit oracle
regenerator; its cargo calls are test tooling, not a product fallback.
