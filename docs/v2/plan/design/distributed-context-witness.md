---
status: current
summary: Key distributed workspace verification by semantic scope inputs and persisted opaque outputs, preserving exact body, source, dependency and proc-module identities across retained and fresh names scopes.
---

# Portable verification inputs

A distributed dev publication is valid for the current checked inputs. A local
`CrateScope` output also retains expansion text from the last names solve. A body
edit can leave its Surface unchanged, correctly keeping that solve, while a fresh
home writes different synthetic block expansion bytes. Those retained bytes must
not split otherwise equal distributed verification contexts.

For a workspace scope, the context uses the task's semantic source and its
sorted, validated cell-read transcript. The existing full source leaves, body
identities and verdicts, graph, scripts, compiler and toolchain inputs remain.
Locked and toolchain crate outputs remain exact. Ordinary runtime `RunIds` facts
and all fetched-entry recipe validation remain unchanged.

The distributed context domain is version 3. Version 2 normalized only the
context's hash transcript; its publications are ineligible for the checked-view
recipe protocol even when all other inputs agree. Scope payload form 8 and
scope source domain 21 retain their existing meaning.

The exporter builds one checked identity containing both the context and its
`RunIds` view. A validated workspace scope witness replaces that view's crate
fact and its matching `today` fact together. The context hashes the view's
actual rows, and planning, the publisher's walk, Maker typing and final placement
all use that same view. A hash-only projection is insufficient: a generic
instance name may contain a workspace ADT whose coarse crate fact contributes to
the recipe checked at placement. Equal publication contexts must not leave that
recipe dependent on the local scope output.

Only the small crate and current-fact tables are copied into the command's bump.
Body identities, checked verdicts, proof slots, read sources and walked edges
remain shared with the original view. A missing witness or an absent or
inconsistent matching current fact refuses the export. Unknown workspace,
locked and toolchain grains remain exact; ordinary run/test and release exports
retain their original view.

The read transcript alone is insufficient for opaque proc macros. The resolver
captures each actual proc expansion's canonical output, including tokens,
origins, outside reads and call identity, after either execution or reuse of a
checked expansion. A scope output witness retains that set and the resolved
link table, which includes macro-valued symbol attributes. It is a new 32-byte
payload section and full-width engine cell. The source domain and payload form
move together; an old or missing witness is refused, never interpreted as an
empty set. Current proc-module output identities also enter the context.

An upgraded binary changes the producer table (`producers.rs`), then the per-kind
tools (`home.rs`). `Pointer::certifies_under` in `engine_answer.rs` compares their
complete digest, so `engine_check.rs` classifies the old answer as `ToolsMoved`
and takes phase admission instead of a certificate hit. The new source domain in
`names::parts` also changes each scope's task key. The engine enters a task whose
key moved (`Cause::KeyMoved`), replacing the form-7 scope with form 8. A prior
form read outside that normal path is explicitly refused; no migration or empty
witness substitution occurs.

The cost is one digest per proc expansion when names actually resolve, 32 bytes
per scope, and a linear walk of the small workspace scopes' existing read lists
when a distributed export is prepared. It does not resolve names again or walk
every body proof. Bounded report receipts retain both the portable witness and
the stored scope output so a mismatch remains attributable.

The regression controls include a real resolver fixture whose standard expression-macro literal
edit preserves Surface and names but changes synthetic expansion bytes; the
portable context must match retained and fresh scopes for that same final source.
Controls move source, private names, dependency reads, scripts, proc-module bytes,
and opaque generated const/static/signature data, origins and outside reads.
Persistence checks refuse the prior form and a missing witness. The
`RunIds` and instance-key control requires raw retained/fresh recipes to differ,
the checked views' recipes to agree, and a changed semantic witness to move the
recipe. Body proofs and closed-grain controls remain unchanged. The complete
distributed acceptance then compares an edited warm producer with a fresh home
and requires zero compilation from its intact cache.
