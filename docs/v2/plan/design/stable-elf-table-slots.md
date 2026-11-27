---
status: plan
summary: Keep ELF relocation and local-symbol records in stable file slots when an edit moves functions, and emit only the changed part of each compared block.
date: 2026-11-12
---
# Stable ELF table slots

Before the reserved-room repair, a same-size body edit wrote 13,402 bytes among
200 filler functions, against 2,498 among 20. The existing constant-size patch
regression failed its unchanged allowance of the smaller patch plus 4,096 bytes. Section
range diagnostics identify `.rela.dyn`, `.blaze.table`, and the `.symtab`/`.strtab`
tail. Removing an old body and putting its replacement in the patch region changes
address order. Rebuilding records in address order shifts unrelated records.
Comparing and emitting whole 4 KiB blocks, then joining nearby writes, adds more bytes.

Keep the existing linker, writer, patch format, digest, and commit path. Give the
ELF writer an explicit mapping from each address-ordered input entry to its file
slot. Retained entries keep their slots. A changed input list can recycle the slots
its splice removed when the new entries fit them: fixed-size relocation records,
and local symbol/name records whose name storage length matches. Assign recycled
slots deterministically. Validate the whole assignment before changing kept state.
Distinguish an unnamed function from one named by an empty string: both take zero
name bytes, but the latter still owns a non-null symbol. Input-position mappings
are permutations of the occupied encoded slots, not new input orderings; address
searches continue to use the sorted input lists. Translate only record writes.
When table cardinality or name storage changes, build the table at its new size.

The ELF constraints remain: null symbol first; local symbols before globals;
`DT_RELACOUNT` leading records all `RELATIVE`; each record occurs exactly once and
names its current address, addend, size, and name. ELF requires neither local
symbols nor relative relocations to be sorted by address. Fresh tables retain their
existing deterministic order. Incremental tables retain explicit slot placement,
as code already retains explicit placement across edits. The exact file digest
continues to bind every emitted byte. A synthesis mismatch after rollback rebuilds
the tables before comparing them with the actual old state.

Relocation multiplicity is preserved: no address-keyed deduplication. Recycle
relative slots only for nonoverlapping eight-byte destinations; overlapping writes
retain canonical loader order. Initialize mappings from the actual encoded order
and bind both mapping and bytes to the same synthesis tag. Failed construction
invalidates that tag. Other writers keep `Delta::patches` unchanged.

A patch region's used size may grow inside its reserved capacity without moving
any file offset. Keep `Delta::same_regions` true for `Class::Patch` and
`Class::PatchData` when region count, class, address, and capacity are unchanged.
Ordinary regions still require equal used size. Keep the separate same-length
splice requirement for writers that need it.

Trim unchanged prefixes and suffixes from compared file blocks before merging
writes. Shared pages remain skipped. This changes emitted ranges, not what bytes
the resulting file holds. Keep the existing 4 KiB comparison and merge bounds.

The budget is the unchanged `a_same_size_body_edit_writes_the_function_and_a_constant`
assertion: body size plus its existing constant, and at most one additional block
between 20 and 200 fillers. Also check table slot reuse directly with thousands of
entries, so byte writes cannot hide an address-order shift beyond the small model.
Mapping computation may still scan input metadata, as current synthesis does;
this change claims bounded emitted bytes, not constant synthesis CPU time.

Prove the remapping against a simple slot model with generated insert/remove/move
splices and repeated edits, including duplicate names, unnamed functions, unequal
name storage, slot reuse, and rejected assignments leaving state untouched. Retain
the existing patch-versus-fresh semantic oracle, file digest oracle, rollback and
torn-patch simulations and their planted-bug controls. Check the emitted ELF
relocation records explicitly: the existing semantic oracle does not establish
that every loader relocation is present exactly once. Execute the affected Rust
tests with Blaze and then the complete linker suite; no lower case counts, raised
write budgets, or compiler/linker delegation.

Keep the existing exact-byte writer oracles for edits that retain input order.
Moved-entry tests additionally compare relocation multisets and decoded local
symbols against a fresh writer, and prove that unaffected records keep their
encoded slots. Cover moves followed by same-position edits and rollback, not just
the first move.

Rewriting canonical sorted tables by narrower byte runs alone was rejected: its
work still grows with the number of unrelated entries shifted by an edit.

The focused repair run passed the unchanged same-size budget regression, seven file
checks, 68 ELF checks, and the thunk control. The two 64-case patch-versus-fresh
and torn-patch properties each reached the 1,800-second bound without a verdict.
They remain unpassed, along with the full linker suite and compiler acceptance.
