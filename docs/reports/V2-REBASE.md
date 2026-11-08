---
status: record
summary: The fork's rebase onto v2 and its handoff: why its builds ended, what was deleted, what became of every fork commit, what ran, what failed, and what is left.
date: 2026-11-07
---
# The fork on v2

The fork's distributed work was replayed onto upstream's `5a7dfc4`, its V1 pipeline superseded by
v2's one build. This says what happened after that replay: how the first stage 1 builds ended, what
was deleted, what became of every fork commit, and what was run. How it works now is
[distributed.md](../distributed.md).

## Why the builds ended

Each cause is from the daemon's own log, the macOS unified log or a diagnostic report.

- **stage1c: memory, on a machine with little swap room.** Its daemon had a 63 GiB limit and held
  70 GiB by its own count when it stopped logging. It ended at 23:47:39.954 (`mDNSResponder` saw pid
  29058 go). `JetsamEvent-2026-11-06-234741.ips` says the machine was out of swap, and that its
  largest process was blaze, at 102.43 GiB counting what was compressed; the report killed 139 other
  processes for `low-swap` and lists blaze as active. So no record names the kill of blaze itself,
  and the daemon's log says only that it ended without finishing its stop. Swap could not grow far:
  the internal disk had 37 GiB free.
- **stage1d: its host, not the build and not memory.** Its log has no `error`. Its task's output
  ends in `[killed]`, written at 08:24:23Z by the delegate's Claude Code process, pid 32428, whose
  own exit is in the unified log that second. The daemon logged `asked to stop by SIGTERM from pid
  32428`, at 56 GiB against a limit of 49, and was gone 1.5 s later.
- **The first stage 1 harness runs: `LOOP_STALLED`.** Two stage 1 daemons, one with the machine
  otherwise idle, ended with exit 76: one turn of the main loop passed the 120 s stall bound with
  every worker idle. It was first read as disk contention with another build, which the second
  occurrence, with 90% of memory free and nothing else running, ruled out.

A build that outlives its host now runs in a session of its own, with its exit status and memory
written beside its log.

## Memory

`BLAZE_DAEMON_MEMORY_GB` bounds the front end, not the daemon. With it at 50 (a limit of 49 GiB), a
fresh daemon's `blaze build -p blaze-engine` reached 84 GiB resident by its own count, mostly the
link's 19 GiB of heap and 6 GiB of tags and the query storage's 18 GiB, and finished in 39m43s on the
64 GiB Mac with 22 GiB in swap. A daemon kept across builds went on growing, to 87 GiB before its next
link; it is stopped between them here.

## What was deleted, and how that was decided

A name was deleted when nothing in `blaze-cargo`, `blaze-dist`, `blaze-dist-local` or `blaze-serve`
reached it outside a test, following field types inside `blaze-wire`. The wire went from 103 types to
43, and the crates from about 26,000 lines to about 19,000.

| What | Why it could go | What holds the behaviour now |
|---|---|---|
| `BuildSpec` and the agent's placeholder | a V2 job is its unit's digest, its platform and its namespace | `JobRequest`, `tests/agent.rs` |
| signatures, capabilities, records, `Hello`, `AgentRequest`, ed25519 | no authentication is built, and nothing signed anything | [distributed.md](../distributed.md#what-it-does-not-do) says so |
| native bundles, observations, runtime bundles, test verdicts, key inputs | V1's saved replay and distributed tests | none: V2 has neither |
| server's `keys/`, `units/`, `deps/`, `.pack.zst`, bare digests, `hello`, `cache/missing`, `objects`, `HEAD`, `Range`, `DELETE`, existence set, key file | the agent calls `PUT` and `GET` of packs and lists | `blaze-serve/tests/serve.rs` |
| `blaze-dist-local serve`, `hello`, `client`, `await_terminal`, `renew` | the harness and the daemon use `coordinate` and the agent | `agent::decide` over the core's `local_action`, `tests/agent.rs` |

What was kept though nothing but its tests calls it: `Subscribe`, `Resume` and `Events` on the
carrier. They are the job core's interface, not V1's, and deleting them would mean rewriting the
core's job tests rather than removing an adapter.

Migrating found four things wrong that deletion alone would have kept:

- A second writer of a list got 409, which the client took as a failure and warned about, not the 412
  it treats as published already.
- A cancelled build never told the coordinator, so its execution stayed until its deadline.
- A pull request's supersession assumed a job is a whole build. A build is many jobs, so an older
  revision's next unit would have counted as a newer push and cancelled the newer build's; such a job
  is now refused, `Refusal::Superseded`.
- A unit with no distributable work, every release unit, started an execution with nothing to do.

## Bugs in v2 found on the way

- **A bin hid its package's library from the package's integration tests.** Lowering made every unit
  dependency a crate, and a test depends on its package's bins for `CARGO_BIN_EXE_*`. With one crate
  name for both, which is the default layout, `blaze test` failed to compile what `blaze check`
  passed. Fixed in `blaze-lower`, with a test and a case in `harness/forkfixes.py`.
- **A script's run did not search where its build dependencies' scripts said** (cargo's
  `add_plugin_deps`). Fixed in `serve/unit/script.rs`.
- **`blaze-cargo`'s unit tests did not compile at upstream's main**, nor did one `blaze-lower` test
  pass: both had fallen behind a change of what they call.
- **`debug = 0` in a dev profile refuses a real crate.** `rustc_type_ir` fails with two
  `Clone::clone`s, of bitflags of different widths, under one store key. Not fixed here: see what is left.
- **The walk of an async body missed the panics of polling it again** (`panic_const_async_fn_resumed`
  and `_panic`): upstream's #1537 changed `v2/mod.rs` and not `v2/refs.rs`. Fixed in b3b5a5041.
- **`use a::r#x; use b::x;` was accepted**, where rustc says E0252: the conflict check took a raw
  identifier's written text as its name. Fixed in 634170a37, with a test.
- **A build held back by memory pressure failed as stalled.** With the system pressed, the plane's
  cap admits nothing new; with nothing running, the 2 s stillness check failed the build and
  stopped the daemon, though the cap passes by itself 10 s after the last event. Fixed in
  `serve/still.rs`, with a test (the commit after c95cd78c1).
- **`blaze-cargo`'s unit tests had rotted.** Beside not compiling, five expected what the code no
  longer says (the tar unpacker's messages since d58797b44, a download line, a ledger record's
  number) or ran only after another test (the JSON status). A raw-identifier test of
  `blaze-diagnostics` named the wrong line (c95cd78c1).

Running the distributed path found these in the fork's own carrier:

- A session journaled every answer and refused a session after 1,024 requests, so an agent waiting
  for an executor, or a holder heartbeating through a long unit, was refused mid-unit. A session
  now keeps its last answer (ed0d8bc4d).
- `blaze-dist-local coordinate` kept the test-sized bound of 4,096 roots and grants an execution,
  so a larger unit waited on `root-capacity` until its deadline (ed0d8bc4d).
- The client refused the answers of `Claim` and `Packs` as unexpected (ad30d46f9), and a unit
  compiled for wasm asked for wasm executors where every executor is the host's (523bddc5b).

## What became of the fork's commits

[V2-REBASE-MAP.tsv](V2-REBASE-MAP.tsv) has every fork commit: its original, what it was rebased
to, a status and why. Of 878: 691 superseded (V2 has the behaviour by its own design, each with
the reason), 65 retained and 103 retained in part, 4 empty, 1 upstream's equivalent, 4 in V2 by a
`harness/forkfixes.py` case, and 10 still owed a result:

| Commit | What is owed |
|---|---|
| 47b6a115c, f36949f1c | fixed here (634170a37); rerun case `raw-duplicate-import` on a stage 1 of this branch |
| cdb01e00b, b96b59521 | cases `attribute-impl-in-function`, `column-counts-scalars` timed out at 1,800 s under load |
| b7f288aa6, 2bf0ffa8f, f715001f4, e26811f32, c69fe89a6, bec070945 | their cases have not run |

Each becomes `in-v2` when its cases print what rustc's program prints, or `gap` naming them. The
cases `a-test-sees-the-library-beside-its-bin`, `a-script-searches-where-its-build-dependencys-script-said`,
`two-derived-clones-with-no-debug-info` and `an-async-fn-and-polling-it-again` hold this branch's
own fixes and have not run either.

## What ran

Stage 0 is `blaze-v0.20261105.4`; stage 1 is this branch built by it with
`CARGO_PROFILE_DEV_DEBUG_ASSERTIONS=false CARGO_PROFILE_DEV_OVERFLOW_CHECKS=false`.

- On stage 0, at dc68fc5f6: `blaze-dist-local` lib, 12 pass (with `a_session_keeps_only_its_last_answer`);
  `blaze-serve` lib, 21 pass; `blaze-diagnostics` lib filtered to the raw and import tests, 5 pass
  (at c95cd78c1). Stage 0 cannot build integration tests whose package has a bin of its own name:
  that is the bug this branch fixes in `blaze-lower`, so they need a stage 1.
- On stage 0, `blaze-cargo` lib: 573 pass, 7 fail. Six are fixed in 181f5d8fc and not
  run since: the five rotted tests above, and this branch's script-search test, which read the
  machine's library-path variable instead of its unit's host's. The seventh,
  `the_explorer_page_is_written_private`, refuses a `TMPDIR` on a volume that ignores ownership,
  as the scratch SSD does: it needs one that keeps it.
- On an earlier stage 1, before ed0d8bc4d: `blaze-wire` 81 pass, `blaze-dist` 96 (the simulation
  over 2,000 seeds), `blaze-dist-local` lib 11, `unix` 22, `protocol` 3, `blaze-serve` integration 10.
  The run on the final stage 1 was stopped by the pressure stall above, since fixed.
- `harness/distv2.py` on an earlier stage 1: solo builds, and its program unwinds a panic across
  units and prints its script's output, an included file and its proc macro's
  (`caught true, dropped 11, script says from the script | included as it was checked in | seen
  by the macro 1`). With two workers, `pm` was executed by b and taken from the cache by a, and
  `shapes` by b with 7 grants of 403 functions and published; then `mathy` waited on
  `root-capacity`, fixed in ed0d8bc4d. No full run has passed yet.
- `harness/forkfixes.py` on that stage 1: 8 of 10 cases as rustc, `raw-duplicate-import` not
  (fixed since), 2 timed out.

## Handing over

Nothing runs on the Mac. Its scratch (`/Volumes/bigssd/projects/blaze-v2-rebase-scratch`, logs and
target directories) stays there and is not needed: what it showed is above, and the map is
[V2-REBASE-MAP.tsv](V2-REBASE-MAP.tsv). On the next host, from this branch, with `B0` a stage 0
blaze and `T` a target directory:

```sh
export CARGO_PROFILE_DEV_DEBUG_ASSERTIONS=false CARGO_PROFILE_DEV_OVERFLOW_CHECKS=false RUST_BACKTRACE=0
"$B0" build --target-dir "$T" -p blaze-engine -p blaze-dist-local -p blaze-serve
"$B0" test --target-dir "$T" -p blaze-cargo --lib
"$B0" test --target-dir "$T" -p blaze-diagnostics --lib
"$T/debug/blaze" serve --stall-bound 1800 &   # an unoptimized daemon needs the longer bound
"$T/debug/blaze" test -p blaze-dist-local -p blaze-serve -p blaze-wire -p blaze-dist --lib --tests
python3 harness/distv2.py '{"blaze":"'"$T"'/debug/blaze","dist_local":"'"$T"'/debug/blaze-dist-local","serve":"'"$T"'/debug/blaze-serve","scratch":"'"$PWD"'/../distv2-run","stall_bound":1800}'
python3 harness/forkfixes.py '{"blaze":"'"$T"'/debug/blaze","scratch":"'"$PWD"'/../forkfixes-run","stall_bound":1800,"timeout":3600}'
```

`XDG_CACHE_HOME` set alike for every daemon shares one machine store of std between them. With
`forkfixes`' `results.json`, each owed row of the map is settled as above.

## What is left

- **Release (LLVM) and folded functions are not distributed**: only a unit's dev functions are, and
  the rest of the unit (most of a build script's instances, say 10,398 of 10,577) compiles where it
  links. The design that closes this is the lead holding its execution through the unit's tail
  and publishing before it ends, helpers waiting for the list, which also closes the next gap.
- **A unit between its execution and its list is compiled again** by a build that submits it then
  ([distributed.md](../distributed.md#what-it-does-not-do)).
- **`BLAZE_DAEMON_MEMORY_GB` does not bound the link's heap or the query storage** (above).
- **`debug = 0` refuses `rustc_type_ir`**: two `Clone::clone`s under one store key, not fixed.
- **Windows-only path identity** commits of `blaze-manifest` are not ported or checked (the map's
  rows say so).
- **What made one turn of the loop take 120 s** (`LOOP_STALLED`) is not known; `--stall-bound`
  works around it.
