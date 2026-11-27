---
status: plan
date: 2026-11-08
summary: Share canonical whole-unit release and fold planning between local codegen and grants; central validation is pending.
---
# Release and folded work distribution

A work set uses the normal driver's fold planner and release summary/import planner before it
names grants. A task is one standalone dev template, one complete folded body with every thunk,
or one release blob with the whole unit's import closure. Equal standalone templates and release
blobs have one representative. Fold-eligible functions that do not join a group remain standalone
tasks. Task names are opaque IDs; a folded task's name covers every member and stored thunk key.

`Build::work_set` now takes a `Lookup` after `check`. Planning reads verified summaries or prepares
them with the existing backend and memo flights. It prepares masked fold IR but emits no code.
Prepared IR/exports are discarded after planning to avoid retaining the whole unit while a holder
waits; a granted task prepares only its own IR and required exports again. This costs a second
prepare on cold holders but no second compiler pipeline or alternate emission path. Local builds
retain their planner's IR and exports, as before. The caller port must land in the same batch.

The unit identity advances to domain 3. It retains all domain-2 inputs (statics, settings, resolved
references and roots) and adds the canonical input summaries and planned task identities. Release
blob keys remain the existing keys over imports, settings, imported units, built-in code and
resolved reference closure. Fold bundles retain the existing body/thunk keys. Each grant exports
all its code values and referenced debug side tables, including warm hits. Missing/corrupt inputs
are errors, including a template-only memo landing whose debug bytes are absent from the reader.
Nothing invents a debug table or silently re-emits code to fill one in. The caller retains prior
grant outputs in the existing `Overlay`, so later grants sharing a memo shape can export its debug
bytes. That per-holder map lasts one unit and refuses conflicting values under one key; it adds no
shared memo or persisted state.

The normal build tail reruns these same planners against granted values and binds stored templates;
it does not emit granted code again. Statics and final assembly remain with the lead. Lease
renewal, stale-generation fencing and atomic full-manifest publication remain the coordinator and
publication lifecycle's responsibility; this change adds no concurrent or durable state.

Driver regressions split a canonical plan over independent mock holders, hydrate an empty store,
and compare its normal assembled output with a cold local build. They cover nonempty release
imports, complete folded bodies/thunks/debug tables, warm re-export, deterministic plans,
equal-input reuse, changed imports/settings/members/references, and invalid grants. A separate
runtime harness patch asks two real holders to compile release and fold tasks, then checks
raw compiled store keys, tail nonduplication, manifests, cold-cache reuse and program bytes.
Root alone runs all validation on the composed tree; this implementation lane runs none.
