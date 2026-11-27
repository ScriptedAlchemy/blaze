---
status: current
summary: Reuse exact in-process read answers before preparing a partially known code entry, retaining temporary coroutine layout preparation and full unknown-answer validation.
date: 2026-11-27
---

# Prepare only unanswered code reads

The [distributed export benchmark](https://github.com/ScriptedAlchemy/blaze/actions/runs/46016703786)
at source `b8d4f94753eb635d90c89fb4ae7bbe3d56eee7b9`
found 10,847 code records on every warm primary export. When the primary had already
verified the final distributed tail, its following reachability walk took 232 ms;
otherwise that walk took 15 s. The retained start records count 14,698 entries verified
from this maker's known read answers in the first case and 3,926 or 3,986 in the others.
These measurements identify existing in-process reuse; they do not measure this change.

`verify_ready` accepts a whole entry when `Answers` holds every exact question and
answer. On a partial miss, preparation omits an exact known non-coroutine row before
decoding its type arguments.
The normal `verify_today` still visits every row, compares known answers, asks unknown
questions, and keeps new answers only after the complete entry holds.

The three coroutine preparation kinds (`Laid`, `CoroutineState`, `CoroutineNiche`) keep
their current behavior on partial misses. Their temporary layouts are forgotten after
verification; an answer retained in `Answers` does not prove that those layouts remain
available to a different unknown question. A whole-entry hit remains unchanged.

The proof remains scoped to the existing maker and its unchanged program and settings.
Peer arrival is not a proof. No persisted format, key, or cross-request cache changes.
The owner-loading path for incomplete verification remains unchanged: a known answer
does not prove that every owner mentioned in its type arguments has been checked.

For an entry with `R` read rows and `K` known answers, lookup remains bounded by
`O(R log K)`. Within `verify_ready`'s preparation loop, decoding and front-end preparation
are proportional to the unanswered preparation rows plus required temporary coroutine
layouts. The profile's "maker read preparation" counters report cumulative prepared and
reused row visits over the maker's lifetime beside the existing whole-entry hit count.
They include distributed completion if this host led it and the following export walk;
a repeated preparation pass counts its visits again. There is no new owned collection or
allocation on the path.

The causal test verifies two entries that share a constant but have another distinct
constant. A fresh maker is the control: it must prepare the shared question again while
the retained maker reuses it. Changed shared and previously unknown constant values must
both refuse the old entry, and the existing read-list edit tests compare cached results
with fresh builds. The async and projection-through-coroutine verification controls
require warm walks to find every record without rebuilding it. Real timing and allocation
claims require a matched benchmark rerun on the final source and binary identity.
