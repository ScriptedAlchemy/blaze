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

The carrier and terminal contract are both version 2. The global codec version is
`blaze_wire::VERSION`. All three must match. A mismatch is refused without trying
another BuildSpec decoder or invoking Cargo or a compiler.

This branch uses global schema 1. C2's separate schema 3 branch changes the global
codec, types and digest domains. Integrating it requires selecting its reviewed
codec and updating the fixtures. Terminal version 2 does not imply global schema 2
or a `/v2` digest domain. Typed digests use the selected codec's domain.

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
`JobContent::of(JobContentInput {spec: digest_of(spec), namespace})`, and compares
the supplied content before submission. No source/materialization or resolved
unit graph is synthesized in this carrier. The compiler/C2 integration is still
a dependency.

## Commands and bindings

The command discriminants are Submit 0, Subscribe 1, Resume 2, Events 3, Cancel 4,
Join 5, Lease 6, Result 7, WorkerEvents 8, KillAck 9, Terminal 10 and Release 11.
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

Creation is idempotent for its nonce while the lifetime is live; after expiry the
same nonce creates a new label and the old one stays dead. A resume names the
issued label and the epoch together; an unknown, expired or released label is
refused, and a different daemon is `daemon-moved`. Session lifetime is fixed, not
renewed by reconnect, and expiry or `Release` returns capacity immediately. The
live-session cap therefore bounds concurrent sessions, never a lifetime quota of
identities, and no tombstone set grows with the number of clients served.

Requests have contiguous sequence numbers starting at 1. Before executing, the
coordinator reserves replay space for a maximum-size response. It journals the
**exact request and encoded answer**, including a refusal, before socket output.
The same sequence/bytes returns that same status without repeating any effects.
Different bytes under the same ID, a gap, exhausted journal or expired session
is refused before transition. Disconnect does not detach a subscription.
After daemon restart, the changed epoch refuses replay; the journal is not
claimed durable across process crashes.

This covers a multi-stage report retried after later stages were already
accepted: it is answered from the journal, not re-fed as backwards progress.
Clients must retain the pending request on IO failure and reconnect/retry it
unchanged. A new session means **new** requests, not the old idempotency lifetime: a
client with an unresolved request must abandon it explicitly rather than replay it
into a fresh lifetime.

The default limits are:

- Frames are at most 1 MiB. The server allows 32 connections and 32 live sessions.
- A session lasts 300 seconds and has a journal of at most 1,024 requests and 8 MiB.
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
blaze-dist-local serve /private/daemon.sock
blaze-dist-local hello /private/daemon.sock
blaze-dist-local client /private/daemon.sock
```

`serve` has no configured worker and no compiler backend; stdin `stop` or EOF
ends it. The embedding API configures admission, builders and a holder/agent
callback. `client` accepts one hex-rendered CBOR Command per stdin line and prints
its CBOR Answer. The CLI does not install a test worker. Shutdown cancels remaining
interests, emits stop effects, closes connections and removes only the
owned socket inode, never another daemon's replacement.

Tests use Unix sockets, the compiled CLI, a callback that stops a process and
the production Jobs/WorkSet code. Their accepted-work fixture reports a Local
item without compiling Rust or importing a pack. Compiler integration requires
a compiler-holder backend and a source/store adapter. Two-worker compilation,
HTTP fleet operation, crash durability and production client races are untested.
