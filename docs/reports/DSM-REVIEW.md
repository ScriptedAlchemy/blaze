# Protocol state-machine review

Review of PR #19, starting at `79c80c7051f9f4fc4982b5ad83b70334c823fa78`, on
2026-10-27. This covers the sans-IO `blaze-dist` protocol core and its deterministic simulation.
It does not claim a working distributed compiler or HTTP integration.

## Corrections

### Assignment identity survives delayed callbacks

Kill effects identify the attempt they target. Admission records both execution and attempt,
and a completion or kill acknowledgment only releases the matching assignment. A duplicated
acknowledgment for an earlier job can no longer free the slot of a newer job on the same
executor. Reassignment emits kills for the previous attempt and reserves the replacement only
after assignment succeeds.

The jobs simulator now delays and duplicates kill acknowledgments, drains those callbacks before
finishing, checks the exact assignment reservation and checks that no live executor leads two
executions. It also checks namespace and resource coverage as well as platform matching.

### Deadlines are independent of timer ordering

Ending an execution first expires each subscriber whose deadline has passed. A completion that
arrives before the next timer tick therefore cannot deliver `Passed` or `Failed` to an expired
subscriber. Longer-lived subscribers still receive the actual result. If the completion arrives
after every remaining deadline, the execution ends cancelled and its subscribers time out.

### Pull-request supersession follows source identity

Platform, command and profile variants of one source revision coexist. A new source revision
cancels all older-source executions with only one attached subscriber; shared executions remain
tracked. Joining an existing shared execution of an older source only adds a subscriber: it
cannot move the current revision backwards or cancel newer work. The regression also covers a
later push after an older shared execution loses a subscriber.

### Replacement leads receive their own compile notification

The `Compiled` stage is recorded per attempt and root-list epoch. A replacement lead must join
with its root set before it can receive that notification. A heartbeat before the join cannot
consume it; retained completion and completion after a root-list reset each notify the correct
attempt once.

### Work-result fencing and lead rejection remain authoritative

A committed result from an older attempt is refused before the replay cache is consulted. A
same-attempt replay still receives its original answer without committing twice. Old traffic
cannot extend a newly joined holder's lifetime.

The lead's rejection list also covers findings discovered before the lead joined, including
waiting, currently held and already exported names. Such names remain in the lead's local tail;
previous or late exports are invalidated. A result for a held name which became rejected or
conflicted acknowledges the name in `WorkResultAnswer.conflicts`, including when the holder
reports `unknown-name`. Its remaining live batch may commit, but the invalidated name cannot
overwrite its terminal state. Replaying the result preserves that acknowledgment without
recounting a commit or conflict.

The first full simulation of the review changes exposed that last interaction at seeds 4722,
6368 and 8034. The trace for seed 4722 had a lead rejection at 25,053 ms and a later, still-live
batch reporting the unknown name at 65,893 ms. The coordinator's early return retained the
rejection but omitted its conflict acknowledgment. A direct regression reproduced the omission
for both rejection and identity conflict. The fix preserves the simulator's original assertion;
it does not excuse an accepted unknown-name result on the grounds of stale-attempt fencing.

### Compiler findings are released across cancellation

Fresh discoveries rejected by the lead or returned as conflicts are released even if they never
entered the holder's accepted-findings map. Stopping while a result awaits acknowledgment also
releases that result's findings. A compile batch that completes after the holder stopped releases
its discoveries without sending another result. Direct holder tests inspect the `Release`
effects, because the simulated compiler does not itself model that retained memory.

## Validation

The focused regression tests were run before their production corrections and failed on their
intended assertions: late kill acknowledgment, deadline delivery, same-source supersession,
replacement-lead notification, old-attempt replay, provisional rejection and the four finding
release cases. The additional invalidated-name acknowledgment regression failed with an empty
conflicts set before its correction. The supersession test also includes old-source
resubscription and preservation of shared work.

Final runtime validation used Rust 1.99.0 on Linux, a dedicated target directory, one Cargo build
job, no incremental compilation, no debug information, and two test threads. From the repository
root, with Rust 1.99.0 and the locked dependencies available, the substantive command is:

```sh
CARGO_BUILD_JOBS=1 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 \
CARGO_INCREMENTAL=0 CARGO_NET_OFFLINE=true RUST_BACKTRACE=0 \
BLAZE_DIST_SEEDS=10000 BLAZE_DIST_SEED= \
cargo +1.99.0 test --locked -p blaze-dist -- --nocapture --test-threads=2
```

Result: **51 tests passed**, no failures or ignored tests.

| Suite | Passing tests |
| --- | ---: |
| Library | 3 |
| Admission | 5 |
| Holder | 4 |
| Jobs | 9 |
| Simulation | 14 |
| Work | 16 |

The simulation passed **10,000 work seeds and 10,000 jobs seeds** (0 through 9,999 for each).
The simulation test binary took 59.66 seconds in this shared Linux environment; that is a test
run duration, not a performance benchmark. Work runs may end with the specified named failures
after lead loss or when no lead remains; they are not all successful compiles. The deterministic
replay equality test also passed.

`cargo +1.99.0 fmt --all --check` and `git diff --check` pass. Four inherited `blaze-wire` files
needed formatting only; no wire behavior or encoding changed.

## Mutation checks

Three independent scratch copies of the corrected `blaze-dist` source were compiled with Rust
1.99.0 against the tested `blaze-wire` library. Each copy changed one protocol rule. A small
driver ran work seeds in increasing order starting at zero, with a bound of 512 and stopping on
the first invariant violation. **These were bounded detection checks, not full 10,000-seed
mutation sweeps.** Production source and the simulator's assertions remained unchanged.

| Scratch mutation | First failing seed | Detected invariant |
| --- | ---: | --- |
| Bypass the live-lease result fence (`if !live`) | 3 | `fenced`: a fenced lease committed |
| Accept a different lead root set without resetting provisional work | 47 | `once`: a root result disagreed with the lead's computation |
| Omit the conflict acknowledgment for an unknown name | 15 | `unknown`: accepted unknown name was not acknowledged as a conflict |

All three mutant libraries compiled, and all three runs returned a specific simulation
`Violation`; detection was not a compilation error or panic. The unmodified code passed the full
20,000-seed suite above.

## Limits before integration

`jobs::local_action` takes only an `Outcome` and treats `Failed` as a completed build/test
verdict. `Jobs` also uses that outcome for `EndWhy::NoLead`. Consequently, calling
`local_action(Fallback, Failed)` for a no-lead infrastructure failure would take the failure
instead of starting a local fallback build. Client integration must preserve the termination
reason and distinguish these cases in its policy decision. The helper's documentation now
states that limit; this review does not redesign the wire or client policy API.

The stage-order simulation checks coordinator-generated stages stored by `Execution`. It does
not validate arbitrary worker-supplied `Stage` events. The seeds also do not exercise an actual
HTTP transport, compiler, process-group driver, store or client race. Those integrations remain
outside this protocol-core PR's scope. No macOS or fleet performance claims follow from these
Linux state-machine tests.
