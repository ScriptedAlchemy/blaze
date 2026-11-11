# Local distributed protocol carrier

This crate connects Unix sockets to `blaze-dist::jobs::Jobs`. `Server` owns a
nonblocking listener and registry. `Client` and the `blaze-dist-local` CLI use
the same API. One reactor owns the bounded connection set without Tokio.
IO, threads and clocks stay outside `blaze-dist`. The carrier does not compile code.

## Framing and versions

Each frame starts with a four-byte unsigned big-endian body length, followed by
exactly one deterministic CBOR value. Zero length, length past `Config.max_frame`,
noncanonical CBOR, unknown fields/variants and trailing bytes are rejected.
Fragmentation and coalescing do not change these boundaries. JSON and hexadecimal
are human renderings only; the CLI renders the actual CBOR bytes as hex.

`protocol.rs` defines the carrier schema. Its structs are integer-key maps and
enums are `[discriminant, payload]`, using blaze-wire's codec/macros. The first
frame is `ClientFrame::Hello(LocalHello)`:

```
LocalHello = {0: transport u32, 1: global_codec u32, 2: terminal u32,
              3: nonce bytes32, 4: daemon_epoch bytes32|null,
              5: resume bytes32|null}
LocalWelcome = {0: transport, 1: global_codec, 2: terminal, 3: session,
                4: daemon_epoch, 5: now_ms, 6: expires_ms,
                7: max_frame u32, 8: next_request u64, 9: created bool}
Request = {0: daemon_epoch, 1: session, 2: sequence, 3: Command}
Response = {0: daemon_epoch, 1: session, 2: sequence, 3: Answer}
ClientFrame = [0, LocalHello] | [1, Request]
ServerFrame = [0, LocalWelcome] | [1, Response] | [2, WireError]
```

The carrier is version 5 and the terminal contract is version 2. The global codec version is
`blaze_wire::VERSION`. All three must match. A mismatch is refused without trying
another decoder or invoking a compiler. Terminal version 2 is not global schema 2:
typed digests use the codec's domain.

### Transport 2: the daemon issues session labels

Version 1 echoed a client-chosen session ID, so refusing to revive an expired ID
meant remembering every ID for the life of the process. Ordinary CLI use minted one
ID per call, and the fixed 32-session cap was therefore a lifetime quota that
exhausted permanently. Version 2 changes who names a session:

- A **creation** Hello sends `nonce` with neither `epoch` nor `resume`. The daemon
  issues a monotonic label and returns `created: true`. Retrying the same nonce
  while that lifetime is live returns the same label with `created: false`; after
  it expires, the same nonce creates a *new* label, and the old label is never
  issued again.
- A **resume** Hello sends both `epoch` and `resume`; naming only one is malformed.
  A resume never creates: an unknown, expired or released label is refused, and a
  different daemon epoch is refused as `daemon-moved`.
- `Release` (Command 11) ends a lifetime deliberately after its answer is journaled.
  The session and its journal are removed, so capacity and memory return at once
  rather than at the fixed lifetime. A release performs no core transition, so a
  client that never saw the answer and retries meets `session-expired` having lost
  only a lifetime, not an effect.

The live-session cap therefore bounds *concurrent* sessions only. Labels are not
capabilities and carry no authority; the epoch check and the existing core rules
do. An older transport-1 peer is refused by the version mismatch, never
reinterpreted as transport 2.


`Submission.request` is an opaque CBOR byte string containing exactly a JobRequest.
The server decodes it only in the negotiated global codec, recomputes
`JobContent::of(JobContentInput {unit, platform, namespace})`, and compares
the supplied content before submission.

## Commands and bindings

The command discriminants are Submit 0, Subscribe 1, Resume 2, Events 3, Cancel 4,
Join 5, Lease 6, Result 7, WorkerEvents 8, KillAck 9, Terminal 10, Release 11, Claim 12 and Packs 13.
Full-manifest acceptance, lookup and delivery are CommitPublication 14, Publication 15 and
DeliverPublication 16. RetireAssignment 17 requests the stop of an owned assignment.
OwnedAssignment 18 reads the current token of an exact owned slot without claiming work.
All payloads and their integer field numbers are in `src/protocol.rs`. All
responses are in `Answer`.

- Submit calls `Jobs::submit`, then admission. Subscribe adds an interest to
  an exact content/execution. Resume returns the same subscription's current
  binding; it does **not** create another subscriber.
- Events pages contain original `EventItem`s after the supplied sequence.
  Cancel calls `Jobs::unsubscribe`; cancelling one interest does not cancel the
  others. An explicitly cancelled subscriber receives a retained Cancelled
  terminal, not a fabricated successful result.
- Join, Lease and Result call registry wrappers around actual Execution/WorkSet
  transitions. Work content, execution, attempt and holder are validated; the
  existing authority/fencing rules are unchanged.
- WorkerEvents invokes the whole-batch ingress validator. Progress is not
  reconstructed by IO, and Passed never bypasses the authoritative work check.
- KillAck carries exact content/execution/attempt/executor and alive status.
  `accepted=true` means an actual pending assignment stop was acknowledged.
  Unknown/stale/duplicate acknowledgments cannot release a new reservation.
- Claim binds its claimant label to the validated transport session. A resumed session retains
  the owner; copying that label into a different session does not acquire the existing claim.
  Promotion keeps the owner of a continuing reservation; fresh or reused slots start unowned.
- OwnedAssignment carries exact content/execution/executor and the claimant label. It reads
  only that owner's current token, including an old pending stop. It cannot claim an unowned
  slot or follow a slot into another execution. Cleanup uses it only for slots the agent ran,
  so cancellation before observing a promotion can retire the current owned token safely.
- RetireAssignment carries exact content/execution/attempt/executor and the claimant label.
  Only that assignment's owner can request its retirement. The coordinator fences its leases
  and findings and emits its normal stop, preserving other subscribers. A retiring lead follows
  the existing bounded reassignment rule. Repeating an owned pending retirement has no effect.
  Capacity remains held until the separate KillAck, after the agent's work has stopped.

Subscription is `{0: content, 1: execution, 2: subscriber}`. Binding is
`{0: Subscription, 1: attempt}`. Response identity must echo epoch/session/sequence,
and `Client` verifies response kind and payload against the pending command.
Terminal lookup carries the exact subscription, returns Pending or immutable
SubscriberTerminalV2, and checks content/subscriber ownership in the registry.
`terminal_action(binding, terminal, policy)` also verifies the exact
attempt before calling typed `local_action`. Mismatched/pending terminals never
start fallback. Resume after lead loss; an already-ended subscriber resumes its
retained terminal attempt even if other subscribers continue on a new attempt.

## Retry, retention and limits

Session labels and daemon epochs are lifetime identities, **not** application
capabilities. The socket is owner-only; deployment access remains the existing
Unix/Tailscale access model. No PR/Main trust/sandbox layer is added.
Within that socket, claims and retirement are bound to the same validated session. The session
binding prevents one session from retiring another's assignment by copying its claimant label.

Creation is idempotent for its nonce while the lifetime is live; after expiry the
same nonce creates a new label and the old one stays dead. A resume names the
issued label and the epoch together; an unknown, expired or released label is
refused, and a different daemon is `daemon-moved`. Reconnect alone does not renew a lifetime.
Ordinary sessions have a fixed lifetime. An accepted job binding can retain its session through
that execution's actual deadline and a cleanup allowance, so a compiler callback cannot outlive
its claim owner or heartbeat clients merely by taking longer than the ordinary lifetime. Each
session also has an immutable ceiling derived from its creation time and the configured limits.
Welcome reports the expiry bound at that handshake; a resume reports extensions accepted since
the preceding handshake. Expiry or `Release` returns session capacity; it does not acknowledge an
executor's stop. The live-session cap bounds concurrent sessions, and no tombstone set grows with
clients served.

Requests have contiguous sequence numbers starting at 1. The coordinator journals the
**exact request and encoded answer**, including a refusal, before socket output.
The same sequence/bytes returns that same status without repeating any effects.
Different bytes under the same ID, a gap, exhausted journal or expired session
is refused before transition. Disconnect does not detach a subscription.
After daemon restart, the changed epoch refuses replay; the journal is not
claimed durable across process crashes.

This covers a multi-stage report retried after later stages were already
accepted: it is answered from the journal, not re-fed as backwards progress.
On IO failure, a client resumes the same session once and retries the exact pending bytes.
If that retry fails, it retains the pending request; it never creates another session to replay
it. A new session means **new** requests, not the old idempotency lifetime.

The default limits are:

- Frames are at most 1 MiB. The server allows 32 connections and 32 live sessions.
- An ordinary session lasts 300 seconds. Its journal keeps only the latest exact request and
  answer, each bounded by the negotiated frame size.
- An execution lasts at most 24 hours from its first queued stage. Submit and Subscribe reject
  deadlines beyond that ceiling before adding an interest. A verified execution binding can
  retain its session through the actual deadline plus 300 seconds for cleanup, within that
  session's immutable creation-time ceiling of 24 hours and 600 seconds.
- The server retains at most 128 executions, each with at most 256 subscribers,
  4,096 events, 8 MiB of public event bytes and 8 MiB of accepted work-result bytes.
- An event page contains 1 to 256 whole events and must fit the negotiated frame.
- A root set or grant contains at most 4,096 items. Excessive job work returns Busy
  without dropping items. This limit belongs to the local transport and does not
  restrict the language the compiler accepts.

Terminal executions stay at least 600 seconds after the reactor observes termination
and **until all assignment stops are acknowledged**. The explicit forget API
releases core event/terminal/result data only then. Pending stops retain their
reservation and can exhaust the finite capacity rather than being evicted.
Expired terminal lookup is a refusal. A previously journaled response can still
be replayed during its own session lifetime. Global frame/request/depth/string/
collection CBOR limits remain enforced.

## Reactor and CLI

`Clock` injects monotonic milliseconds relative to daemon creation. Production
uses Instant, never wall time. Hello reports that origin; submit deadlines are
daemon-relative. Clock rollback is clamped, never extending a lease. Nonblocking
read/write and one frame per connection/pass keep timer progress independent of
slow clients. Production `Effects` callbacks must enqueue backend work promptly.
The carrier delivers actual effects once, but never auto-acknowledges a kill.

```
blaze-dist-local coordinate SOCKET TRIPLE EXECUTORS
```

`coordinate` is the coordinator of a machine's daemons: EXECUTORS builders of one host triple,
until stdin says `stop` or closes. Shutdown cancels remaining interests, emits stop effects,
closes connections and removes only the owned socket inode, never another daemon's replacement.

`agent.rs` is what a daemon runs, and `tests/agent.rs` runs it over the real carrier and the real
cache server with a batch compiler standing in for the code generator. `tests/unix.rs` uses Unix
sockets, the compiled program, a callback that stops a process and the production Jobs/WorkSet
code. `harness/distv2.py` runs real daemons. HTTP fleet operation and crash durability are
untested.
