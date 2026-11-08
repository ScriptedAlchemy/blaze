# distv2 acceptance

`../distv2.py` builds the fixture with real Blaze daemons and checks actual artifacts and native
server packs. It never creates acceptance artifacts itself. `selfcheck.py` exercises only the
Python evidence oracles with small synthetic inputs; passing it is not distributed acceptance.

A run needs a **new, empty scratch directory**. Earlier runs, logs and artifacts are never wiped.
It retains `provenance.json` (source commit/tree/patch, fixture digest, all three binary hashes,
input and effective resources), `source.patch`, child logs, reports, events,
`artifacts.json` (original path/output/exit/hash), and the actual server's packs/manifests. Source and
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
- `publication_attempt`: the attempt that first accepted the immutable manifest; it can be
  older than `attempt` when a replacement lead delivers that accepted record.
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
| `done` | Coordinator, only after accepting a real terminal Done: `unit_digest`, `execution`, `attempt`, `outcome` (the actual outcome name, `Passed` for success). |
| `published` | Publication owner, after a full manifest is accepted: `unit_digest`, `execution`, `attempt` (the original accepting attempt), `packs` (accepted pack hex digests). Delivery can use an equivalent cache partition; the oracle verifies every pack hash and compares all logical key/kind/byte values, while requiring Done after acceptance. |

An identity is `(execution, attempt, holder, generation, seq)`; `work` uses protocol WorkNames,
while report arrays use store keys. Do not substitute unit names, ordinals, hashes of log messages,
or counters. Stale-fault acceptance stops an actual compiler process holding a grant, waits for
replacement **accepted commits of every outstanding name in the same execution**, then resumes it
and demands a refusal of that exact stale batch and no accepted commit from it. The explicit `BLAZE_DIST_TEST_RESULT_GATE` hook on the victim daemon captures the
actual Holder-produced `Command::Result` immediately before transport. It selects only the current
lead, and stops its driver and heartbeats at a bounded gate, so the coordinator advances its attempt
before the resumed carrier is sent. The production path with this setting absent performs no capture
or gate IO. The harness also releases a real captured result before expiry as a positive control.
Both controls require the normal Client's actual answer and the coordinator's corresponding event;
neither a discarded result nor a `holder-lost` event can satisfy the stale case. Worker-death recovery likewise requires real replacement
commits; an unrelated completed unit is insufficient.

The full manifest must name the current attempt's work plus its tail, with Done delayed until
publication/mirroring. Acceptance checks that successful Done follows the actual manifest commit
in the shared event stream. The native list remains `pack_digest byte_size keys_digest` per line;
`packs/<digest>.pack` remains the native `blzdpk\0\x01` format. The harness reads these actual files
and chooses a referenced, verified pack to corrupt. Intact → corrupt/Never refusal → explicit
Fallback correctness → restored/Never success is the causal control; no empty glob can pass it.


## Explicit real-result test gate

Set `BLAZE_DIST_TEST_RESULT_GATE` only on the victim daemon to a **new empty absolute directory**.
The harness controls this setting directly and never inherits it or accepts it through input `env`.
One current-lead holder wins an exclusive `claimed` creation after its real result exists. Before
any send it writes the exact canonical `Command::Result` to `result.cbor`, then atomically names
`ready.json`, a view of those same CBOR bytes with real pid, lease/batch identity, ordered WorkNames,
`command_sha256` and the full wire JSON rendering under `command`. It appends `result_held` with
the same identity, pid, work and hash to `BLAZE_DIST_EVENTS`. No authority is created by this record.

The harness binds these values to an actual preceding `grant`, stops that process, and waits for
replacement accepted commits covering every captured name in the same execution. Creating `release`
and resuming the process lets the original unchanged result pass through `Client::call` and the
normal server route. A fresh transport session, created before any request is pending, prevents an
expired idle connection from hiding the work lease refusal; sessions carry no work authority. The
real response is retained as `answer.cbor` and atomically named `answer.json` (`sha256`, `answer`).
The stale case requires both the real typed DeadLease/AttemptMoved answer and `result_refused` for
the captured identity, and still forbids any accepted commit of that identity. The positive stage
uses a different edit and gate, releases without expiry, and requires Result plus `result_accepted`.

The gate expires by the unit's finite deadline, including SIGSTOP time, using the deadline wait
helper and a span naming the directory. Failure leaves `error.txt`, removes a partial temporary
view, and never sends a fabricated answer or event. Run cleanup releases every owned gate before
resuming and terminating children; captures remain in scratch as evidence. A claim is never reused.


## LLVM and folded-group distribution

After applying the release/fold driver and caller patches, apply the separate runtime extension
before building stage 1. Set `release_fold` to true, `args` to `["--release"]`, and `profile` to
`"release"` in a fresh typed input. Preserve root's measured resource settings and time bounds.
The proc macro executes 64 non-inlined wasm bodies with distinct constants against an independent
loop oracle; the application and native dependencies exercise LLVM at release settings.

`backend_work` events name the real daemon pid, unit digest, granted protocol WorkNames, raw compiled keys,
and the intersection of those emissions with the canonical release and fold output keys. The
oracle joins them to real accepted grant/lease events and final reports. Each backend kind must
have two distinct emitting daemons within one successful unit/execution/attempt; merely planning
work, cached hits, another unit, or stale refused results cannot satisfy it. The existing raw-key,
manifest, tail nonduplication, cold-cache, edit and executable-byte oracles still run. Missing
folding opportunities or unsupported LLVM are failures, never skips or alternate backends.

The extension has not run in the implementation lane. Root owns preflight, build and acceptance.
