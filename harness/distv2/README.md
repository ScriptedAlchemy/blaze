# distv2 acceptance

`../distv2.py` builds the fixture with real Blaze daemons and checks actual artifacts and native
server packs. It never creates acceptance artifacts itself. `selfcheck.py` exercises only the
Python evidence oracles with small synthetic inputs; passing it is not distributed acceptance.

A run needs a **new, empty scratch directory**. Earlier runs, logs and artifacts are never wiped.
It retains `provenance.json` (source commit/tree/patch, fixture digest, all three binary hashes,
input and effective resources), `source.patch`, child logs, reports, events, executable copies by
SHA-256, `artifacts.json` (output/exit/hash), and the actual server's packs/manifests. Source and
binaries must remain fixed during acceptance. A failed stage stops all owned processes, including
suspended workers; each build and fault-evidence wait has a bound. Idle daemons exit between
stages so the run keeps at most two front ends resident. No compiler or daemon runs in preflight.

Input is one JSON object, a JSON file path, or `-` for stdin. `cores` and `daemon_memory_gb` are
positive integer **per-daemon** hints. They must be supplied through typed input, explicit `env`,
or the caller's environment before any children start. Typed hints override `env`, which overrides the
caller's explicit `BLAZE_CORES` / `BLAZE_DAEMON_MEMORY_GB`. There are no default resource caps.
`env` carries explicitly selected toolchain/runtime settings; inherited `CARGO*`, `RUST*` and other
`BLAZE_*` settings are isolated. Do not put distribution/policy settings in `env`: those are the
cases' controlled inputs. Two daemons need a combined budget selected from measured free memory,
not just the front-end hint (which does not bound the whole daemon).

Run only after root supplies stage1-ready and a slot, with its baseline daemon stopped. Use the
integrated source tree that built the three binaries, not the frozen bootstrap source checkout.
Root should derive a fresh input from `/fast/projects/blaze-v2-linux-state/distv2-input.json`, set
a new `scratch`, leave `faults` true and supply measured resource hints, then run:

```sh
python3 harness/distv2.py /path/to/new-distv2-input.json > /path/to/distv2-acceptance.log 2>&1
```

Lightweight preflight (no Blaze binary or daemon is started):

```sh
python3 -m py_compile harness/distv2.py harness/distv2/evidence.py harness/distv2/selfcheck.py
python3 harness/distv2/selfcheck.py
```

## Required product telemetry (API request to root)

The exact base does **not** supply the telemetry below. Root must coordinate these additions with
the compiler driver and the existing publication/lifecycle owner before full acceptance. The
harness deliberately fails on missing evidence; these are not optional or skipped assertions.
This is observation of the existing pipeline, not another compilation or protocol implementation.

`BLAZE_DIST_REPORT` retains its existing JSONL fields and adds:

- `unit_digest`: lowercase 64-character digest used for `manifests/<digest>`.
- `attempt`: final successful attempt number, or `-1` when no execution was joined.
- `work_compiled_keys`, `tail_compiled_keys`: arrays of actual compiled **store template keys**
  as lowercase hex, counted by `compiled_for_work` and `tail_compiled`, respectively. Include every
  real compile, even a duplicate; do not deduplicate these arrays or include store hits. These let
  the oracle reject work/tail overlap and duplication across daemons for the same unit. They also
  prove every compiled value appears in a real pack of the full published manifest.

`BLAZE_DIST_EVENTS` names **one shared append-only JSONL file** passed to daemons, clients and the
coordinator. Each event is a single complete append followed by a newline, visible promptly (not
buffered until unit completion). Concurrent writers must not interleave a record. Required events:

| `event` | Emitter and required fields |
| --- | --- |
| `grant` | Agent, immediately before actual batch compilation: `pid` (daemon), `unit_digest`, `execution`, `attempt`, `holder`, `generation`, `seq`, `work` (exact granted WorkName hex digests, expanding root ranges against the fixed root set). |
| `result_accepted` | Coordinator, only after an actual batch result commits: `execution`, `attempt`, `holder`, `generation`, `seq`, `work` (only successfully committed names). Retries may repeat the same identity. |
| `result_refused` | Coordinator, after refusing an actual stale batch result: the same lease/batch identity and `refusal` (`DeadLease` or `AttemptMoved`, using the real typed reason). |
| `done` | Coordinator, only after accepting a real terminal Done: `execution`, `attempt`, `outcome` (the actual outcome name, `Passed` for success). |
| `published` | Publication owner, after the current attempt's full manifest is committed: `unit_digest`, `execution`, `attempt`, `packs` (all pack hex digests, including tail/mirroring). Emit only for an actual successful publication; reports finish after this boundary. |

An identity is `(execution, attempt, holder, generation, seq)`; `work` uses protocol WorkNames,
while report arrays use store keys. Do not substitute unit names, ordinals, hashes of log messages,
or counters. Stale-fault acceptance stops an actual compiler process holding a grant, waits for
replacement **accepted commits of every outstanding name in the same execution**, then resumes it
and demands a refusal of that exact stale batch and no accepted commit from it. If the existing
holder cancels/discards the batch without submitting it after resume, the strict stale-result
stage is blocked: root must resolve how to exercise the real server refusal, not fabricate an event
or weaken it to observing `holder-lost`. Worker-death recovery likewise requires real replacement
commits; an unrelated completed unit is insufficient.

The full manifest must name the current attempt's work plus its tail, with Done delayed until
publication/mirroring. Acceptance checks that successful Done follows the actual manifest commit
in the shared event stream. The native list remains `pack_digest byte_size keys_digest` per line;
`packs/<digest>.pack` remains the native `blzdpk\0\x01` format. The harness reads these actual files
and chooses a referenced, verified pack to corrupt. Intact → corrupt/Never refusal → explicit
Fallback correctness → restored/Never success is the causal control; no empty glob can pass it.
