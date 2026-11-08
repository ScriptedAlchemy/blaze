---
status: plan
summary: Complete distributed unit identities include function templates, static templates, resolved references and roots; verification is pending.
date: 2026-11-08
---
# Complete distributed work identity

A unit's published list supplies store values to the existing compiler. A digest of only the walked function keys omits static-only units, changes to static values and settings, and root selection. Such units may otherwise select the same published list despite needing different values.

The unit domain advances to `blaze dist unit 3`. Its ordered input is the complete walk: each function's instance and template keys, each static's item/instance and existing static-template key, the resolved targets of both, and the function roots. Each collection is length-delimited. Node/static lists use the walk's existing key order, and roots keep the compiler's supplied order. The existing key helpers remain the authority for a template's inputs. The digest also covers canonical release summaries and planned standalone/release/fold tasks, as described in [release and fold distribution](release-fold-distribution.md). The static walk need not remain allocated after its identity is computed.

Hashing just distributable dev functions is insufficient. The complete list also carries the ordinary build's release/fold/static values, and cannot be keyed by a subset of its inputs. This changes the cache namespace rather than treating old lists as complete.

Verification: two mock-front-end tests keep the function list empty while changing a reached static's value, section and settings, with an unreachable-static negative control; and keep the reached function set equal while changing root selection, with a deterministic repeat control. Run these against the old identity first, then the changed identity. Runtime tests must also edit an included/static input and verify published-cache reuse cannot return stale program bytes. No tests have run on this candidate yet.

Standalone dev work is granted once per existing template key, with the first instance in the sorted walk as its representative. Distinct instances may bind the same stored template to their own references and source tables; granting both would allow two remote holders to compile the same template. The normal build still sees every instance. A mock-backend control grants the shared template once, fills the store, then checks that assembling both instances emits nothing else.
