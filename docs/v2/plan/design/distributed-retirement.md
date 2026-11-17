---
status: plan
summary: An owned assignment retires through an attempt-fenced stop request and a separate acknowledgment; runtime verification is pending.
date: 2026-11-11
---
# Retiring a distributed assignment

A subscriber can leave while another requester still needs the execution. Its agent has stopped
working, but cancellation of that one subscription does not stop the execution's helper. A
`KillAck` without a pending stop is correctly refused, leaving that helper's reservation held.

Local transport version 5 adds `RetireAssignment`, naming content, execution, attempt, executor
and claimant. The coordinator derives the claimant's core identity from the validated transport
session and its claimant label, both on claim and retirement. A resumed session retains ownership;
a different session cannot copy the label and retire another owner's assignment. The existing
assignment registry stores the token, owner and pending-stop state in one place, including after
lead replacement, until acknowledgment releases it.

Promotion preserves ownership when the same executor continues in the same execution: its
reservation and local worker did not stop. Fresh or reused assignments never inherit that owner.
An exact-slot `OwnedAssignment` lookup returns the current token only to that owner, without
claiming any work. After its scopes stop, cleanup looks up only executors the agent actually
claimed, retires the returned token and separately acknowledges its stop. If promotion races
that request, the stale token is refused and lookup repeats within the registry's two-attempt
bound. This closes cancellation before the agent observes its promotion. The session stays live
through cleanup; ordinary `Release` still removes only the carrier lifetime.

Long compiler callbacks use independently connected heartbeat drivers, so the ownership client
can exceed its socket's idle timeout. Its next request resumes the same session once after an IO
failure and retries the exact pending bytes; it never migrates that request to a fresh session.
Opening or reopening the Unix socket has a five-second absolute deadline. A full listener
backlog is a named refusal, and every unsuccessful connection attempt closes its owned descriptor.
Verified execution bindings retain a session through the actual execution deadline and a
bounded cleanup allowance. The default execution ceiling is 24 hours from first queueing and
the cleanup allowance is five minutes; overlong Submit or Subscribe deadlines are refused
before mutation. An immutable session ceiling also bounds later binding extensions. Ordinary
unbound sessions retain their fixed lifetime, and genuine expiry cannot be revived.

Retirement validates the exact owned assignment before changing it. An already-pending stop of
that token and owner is idempotent. A stale token or wrong owner is refused without changing the
new assignment. For a helper, retirement fences its lease and findings through the existing
work-set rules, removes execution membership and excludes the slot from that execution's future
helper offers. For a lead, it uses the existing bounded lead-loss and reassignment transition.
Other subscribers retain their own deadlines and terminal outcomes.

The transition emits the ordinary exact-token stop. It does not release capacity. After every
local compiler and heartbeat scope has stopped, the agent leaves its subscription on failure,
requests retirement of its observed assignments, and separately acknowledges the stops. Existing
`KillAck` fencing remains the only release authority. Accepting an unmatched acknowledgment was
rejected because it could free a replacement assignment whose work is still running.

The cost is a bounded lookup and retirement request per observed assignment, repeated only when
the execution's existing bounded reassignment races cleanup, and one owner field in the existing
assignment record; the old per-execution claim map is removed. Lease/finding cleanup uses the
same bounded work-set operations as holder loss. Old transport peers fail version negotiation;
the global codec, terminal contract and immutable pack format do not change.
The carrier's finite execution-binding set and latest-request journal bound retained session
state; reconnect adds at most one same-session retry to a failed call.

A stale claimed helper cannot be taken over without proof that its old work stopped. Replacement
prefers helpers with recent heartbeats; the fallback for an assigned but unstarted helper is
limited to unclaimed reservations, before trying an idle executor. A stop of a dead worker still
needs the existing driver's acknowledgment. Session expiry or release is not proof of a stop.

The Hegel simulation drives the real registry with helper departure, delayed retirement and
acknowledgment, lead reassignment, late results and slot reuse. Its oracle requires a surviving
subscriber to remain attached, the retired helper's roots to be granted under a fresh lease,
one live owner per assignment, no capacity return before the exact stop acknowledgment, and no
effect from stale retirement or acknowledgment. Planted missing owner, attempt, stop and work
fences must violate it. Carrier tests exercise same-session ownership,
replay, exact-slot lookup without claiming, cross-session refusal, cancellation before observing
promotion and a cancelled helper while its lead continues. These checks have not run on this
candidate.
