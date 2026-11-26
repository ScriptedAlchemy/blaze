---
status: record
---

# Coroutine layout during constant evaluation on e676

The isolated baseline is `e6768d0f2ed97e2713a183b49ac8476ba585e7b8`.
The retained `an-async-fn-and-polling-it-again` fixture fails while evaluating
`SizedTypeProperties::IS_ZST` for the closure capturing the pinned future.
Constant evaluation lacks the opaque and coroutine layout facts used by runtime
building. The fixture must keep both its result `5` and its panic payload after
polling the completed future.

The port passes build facts through `FrontEnd::const_at`, supplies opaque hidden
types and coroutine shapes to `ConstsWorld`, and matches coroutine arguments
structurally across the evaluator and build type tables. State layout precedes
constant evaluation and uses the runtime layout implementation. Referenced
coroutines are discovered through pointers without recursively expanding all
pointee ADT fields. No separate const-only layout rule is introduced.

This scopes Andrew Gazelka's upstream changes from `20c8ab3f14`, `19855168df`,
`7420af6f4`, `d823038eae`, `52106b401b`, and the diagnostic portion of
`214afb3c44`. It excludes unrelated solver, borrow, codegen, and maker rewrites.
The e676 maker retains its existing bounded work loops and constant-table API.
The state-retention portion of `214afb3c44` depends on the newer on-demand
coroutine builder in `a5b3c536d2`; retaining states without that builder would
skip building coroutine functions. This port continues forgetting temporary
const-layout states before the ordinary builder runs.

Validation is performed in `/fast/projects/blaze-pr81-async-is-zst-e676` on
ubuntu-main with target directory `/fast/targets/pr81-async-is-zst-e676` and
`RUST_BACKTRACE=0`. The initial interface port passed
`cargo +nightly-2026-10-27 check -p blaze-ladder --tests` in 55.69 seconds.
The final source snapshot also compiled the `front2_closure_consts_run` test
executable in 2m15s. Running its six tests on x86-64 Ubuntu gave one pass (the
compile-time rejection control) and five failures, all at the existing platform
boundary: `the trap watch: a trap's context is read on AArch64 macOS and Linux
only`. The new coroutine constant-layout regression was one of those five.
This is not a successful async behavior result. The next runtime proof needs
an AArch64 venue or the separately integrated x86 trap-context support; no
platform workaround is included in this layout port.

The later `046c522f4c` nested-refusal change also assumes a newer
`MakeFail::Hidden`/prepared-body interface absent from e676 and is not included.
No success is claimed for the retained end-to-end fixture or the full gate.
