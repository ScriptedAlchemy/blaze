# Recorded callable corpus

From `crates/hir-ty-pure/src/callable/`:

```sh
python3 -B corpus/generate.py '{"schema":"callable-fixture-source/1","mode":"check","output":"fixtures_data.rs"}'
python3 -B corpus/generate.py '{"schema":"callable-fixture-source/1","mode":"generate","output":"fixtures_data.rs"}'
```

The one JSON request may also be a file path or `-` for stdin. Unknown fields,
duplicate JSON fields, invalid field types, unknown schema/mode, missing explicit
output, and output inside `corpus/` are refused. Check mode only reads inputs and
compares the specified output byte for byte. Generate mode writes only that
explicit output file; its parent must exist. There are no dependencies beyond
Python's standard library and no working-directory dependency for input lookup.
The table has `#[rustfmt::skip]`; support code is formatted separately.

`inputs.json` bundles the original JSON values for all 20 independent analyzer
inputs, the authored corpus, recorded actual results and checkout identity.
Re-encoding each value as sorted, two-space JSON with one trailing newline
recovers its exact original bytes and SHA256. The original sealed manifest is
retained to verify those hashes and identify omitted evidence, including the
oracle code; omitted files are neither loaded nor needed to regenerate the table.
The generator seals the entire bundle and verifies all authored expected fields
against the recorded actual objects, including failure details and finite
prefixes. Corpus execution payloads and actual traces never enter `Adapter`.
No interpreter, solver, native binary, API implementation, or old source checkout
is included or used. The adapter class and expectation policy are reused from
the original generator, identified in `inputs.json`; its graph rows are unchanged.

Policies stay explicit: 11 records require closure, eight require an open result,
and `fuel_exhausted` permits either. A closed result must include all recorded
invoked targets and effect bits (`io.write = 1 << 0`, `unwind = 1 << 1`). Opaque
inputs/cells stay opaque; external callback targets are never inferred from
concrete payloads. Exposure/recovery retain their explicit unsupported-validation
obligations. Both branches and all function bodies remain in the graph. Empty
cells remain empty until a store. Missing-target, missing-effect and
all-incomplete controls check the same assertion predicate; these Python checks
do not run a graph solver. Failed/incomplete results witness prefixes only.

The frozen source identity is `bef2422829b778559c292c4380d32d0953691529`, tree
`dadd7013ae0f552e6058e384e29dfe0569f3c91d`. The original oracle actually observed
checkout `c531b3b4cdae2c4e2543e6e274cb3f37dc240ff0`, tree
`9ac3e4e041f96c8a1c9ba1fed76cc6c7819f15c7`; those identities differ. The frozen
identity describes source evidence, not the checkout on which the oracle ran.
An accepted analyzer input schema is not evidence of native acceptance. Nothing
here establishes production/MIR extraction, Rust pointer validity, termination,
native compilation, or execution of the integrated Rust fixture.

The hand-written support and controls are in `../fixtures.rs`, which includes the
generated table. The external validation receipts are not generator inputs.
