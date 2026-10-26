# The front-end image: what a worker starts its build with, and how much of it a file can hold

2026-10-26, for [#274](https://github.com/indexable-inc/blaze/issues/274) (state as an image) and
unit D3 of [distributed.md](../distributed.md) (the front-end image), with frozen dependency
artifacts ([#40](https://github.com/indexable-inc/blaze/issues/40)) as its first slice. Measured on
a Mac (Apple silicon, 18 cores, 48 GB, shared with other units' builds). This lane starts at
`f863483f` on `dist/main`. The prepared fixtures are the `harness/dist/fixtures.toml`
pins of ripgrep 15.1.0 and nushell 0.108.0; their measurements are not yet available.
Each claim is **read** (measured, or read in a source),
**reported** (someone says so) or **unverified** (our inference).

## Short version

The first slice maps the existing frozen bytes. It does **not** map decoded def maps,
macro expansions or bodies. It cannot remove the reported 7.9 GB front-end heap by itself.
The six reported holders below account for 64% of a tracedecay sampled heap, not 64% of
physical footprint. Their dependency/workspace byte split remains unmeasured.

The predecessor measured clean-page reclaim in a bounded Linux container with synthetic
bytes, not a blaze worker: three mapped runs survived, three heap runs were killed. That
is evidence for the OS mechanism, not a blaze speedup or a complete #274 implementation.

## 1. What the 64% is

The question: a tracedecay worker held to 16 GiB ran out of memory before `compile_ahead` was
done, and 64% of its heap at the bound was what the build starts with
([LOADMEM.md](../reports/LOADMEM.md), section 8, run A3, one run). Which ingredients are that, which
crates they belong to, and which of them stay what they are once computed?

### What the worker held at the bound (tracedecay)

Run A3's last heap profile, 14,563 MB live, 12,214 MB sampled by jemalloc (**read**, from
LOADMEM.md and `docs/reports/loadmem/u7-a3-holders.md` on `dist/u7-bound`; one run). The
columns that are not in that report are this unit's, and are marked as to how they are known.

| structure | MB | its ingredients | keys | changes with an edit of the workspace? |
|---|---|---|---|---|
| macro expansions | 2,029 | `parse_macro_expansion` 1,433, `macro_arg` 139, `expand_proc_macro` 117, hygiene | one per macro call: 19,987 proc-macro expansions of which 16,480 derives, and every `macro_rules!` call | a call in a dependency: no. A call in a workspace file: yes, with the file. A derive: with its item |
| bodies, signatures, item trees | 1,985 | `Body::with_source_map` 612, `Body::relative` 395, `body_syntax` 156, `ImplSignature` 141, `FunctionSignature` 84, `file_item_tree` 84 | per definition with a body, per item | in a dependency: no. In the workspace: yes |
| file parses | 1,547 | `EditionedFileId::parse` 1,411, ast id maps 77 | one per loaded file | the file's own edit only |
| symbol table | 1,198 | `SymbolTable`: 1,019,117 symbols of the objects and rlibs cargo built, about 1.2 KB each | one per symbol | the workspace's objects: with every build. The dependencies' rlibs: no |
| name resolution | 669 | `crate_local_def_map` 151, `ModuleScope::name` 105, imports | one per crate, one per module | a dependency's: no (the frozen path serves it). The workspace's: yes |
| file texts | 450 | vfs: `FileText` | one per file | the workspace's files: yes |
| **the front end the build starts with** | **7,878** | | | |
| MIR, code, inference, layouts, driver | 3,178 | `runtime_mir_body` 348, `mir_body` 264, `compiled_instance` 390 + 153, `InferenceResult::for_body` 513, `inlinable` 135, driver 441, layouts 223 | one per instance | the instances of a changed body and what calls it |
| ra-jit's own and salsa's other | 1,152 | | | |

The 64% is the six holder rows (7,878 of 12,214 MB, **read**). The build adds the rest in 27
seconds, and some of the front end too: bodies and signatures were 743 MB at the first profile and
1,985 MB at the last, because codegen lowers the bodies it asks for (**read**).

**Unverified split.** The historical sampled profiles are grouped by allocation stack,
not crate. They do not establish how many bytes belong to dependencies. Do not extrapolate
file or crate counts into a measured heap share. `RA_JIT_FRONT_STATS=census` adds counts of
crates, modules, distinct module-origin files *within each crate*, and their source bytes
for eligible frozen dependencies, other external dependencies, and workspace crates.
Files shared across crates/configurations may count more than once; expansion files and
body/signature heap bytes are not counted. Use that census as a coverage check before adding
per-owner allocation attribution. The dependency part is immutable only for the complete
frozen key (including cfg, environment and build-script output), not across key changes.

## 2. What can be laid out to be mapped, and read

### What exists

- **Frozen parts** (#40; `crates/ra-jit/src/frozen.rs`, `hir-def`'s `nameres/frozen.rs`,
  `hir-ty`'s `method_resolution/frozen.rs`). Three parts per crate whose source is the
  toolchain's, a registry's or a git checkout's and whose dependencies' are too: the `DefMap` with
  its `LocalDefMap`, the impls, and the lang items, each an entry of the store under the crate's
  Merkle key (the source's path below the registry, edition, cfg, environment, the content of what
  a build script wrote, the keys of its dependencies). The bytes name no salsa id: a file is its
  crate and its path, a module its crate and index, an interned id its location, a macro call and a
  syntax context are written once and named by index. They are position-independent (**read**).
- **But the read is a decode.** The format is a serial stream of LEB128 numbers and inline strings
  in the order of the keys' fingerprints (so the bytes are the same in every process). There is no
  index into it, so nothing in it can be read without reading what is before it. `Dec` interns
  every name and location again (`Symbol::intern`, `Loc::intern`) and builds the `DefMap`
  in the heap, which `crate_local_def_map` memoizes (`hir-def/src/nameres.rs:485`) (**read**).
  What the store hands over is a copy: `Store::get` returns a `Vec<u8>` out of a zstd block of at
  least 128 KiB, through a 32 MiB cache of unpacked blocks
  ([concurrent-reads.md](concurrent-reads.md)), and `FrozenDefMaps` keeps it, an `Arc<[u8]>` of
  every served crate, in a salsa input for as long as the worker lives (**read**; this prototype
  changes that byte owner to an owned buffer or an `Arc`-owned mapping range).
- **Flat syntax trees** (#42, [flat-syntax.md](../flat-syntax.md)): arrays in preorder, 14 bytes an
  element (kind `u16`, start, end of subtree and parent `u32`), the text of tokens a slice of the
  file's text. Nothing in them is a pointer or an id (**read**).
- **Not serialized anywhere**: item trees, signatures, bodies, macro expansions, MIR, inference
  results. Distributed.md says the same of MIR (`no serialised MIR exists`) (**read**).
- **salsa has no way to install a memo that borrows from a mapping.** Its `persistence` feature
  deserializes owned values; the function that stores a memo (`vendor/salsa/src/function.rs:328`) is
  internal (**read**, not tried).

### The three levels of reading, and what each takes out of the heap

1. **Mapped bytes, decoded when asked** (the first slice, built here). The bytes of a frozen part are
   a range of a mapped file. The `DefMap` is decoded into the heap as before. What leaves the heap is
   the encoded bytes (and the copy through the store); what stays is everything decoded.
2. **Addressable sections with a translation table.** A section has an index (module, body,
   expansion by number); a process reads one on demand and interns what it names, keeping only a
   `Vec<u32>` per section of image index to the process's own id. A decoded value is a cache: it may be
   evicted and read again (salsa `lru`, which evicts at the start of a revision only, or a cache of
   ra-jit's own). Heap is what a build touched, not what exists.
3. **Plain data read in place**: file texts, flat trees, a symbol table. No ids, so no table: a
   salsa input whose value is a view of the mapping, returned by reference. No decode, no memo, no
   record.

### How salsa reads it

- **A hook at the top of the query**, which the frozen parts use (`served()` first thing in
  `crate_local_def_map`, `lang_item.rs:40`, `method_resolution.rs:932,1059`). Right for level 1 and
  2: the memo is the decoded value. A query whose value is a view makes level 3 only if the view
  is what the memo stores, which is a pointer into the mapping and a memo's worth of salsa record.
- **An input whose value is the view** (the singleton `FrozenDefMaps` at `Durability::HIGH` is one).
  Queries return it by reference; salsa records one edge to the input per reader, and with
  `HIGH` the verification of a workspace edit skips it (**read**; lean-frontend.md found that
  durability alone did not shorten edits, which is about time and not about this).
- A genuinely view-returning query can avoid owned dependency payloads, but it still has a memo
  and dependency records. Neither HIGH durability nor this byte-mapping prototype removes those
  records. Salsa's records are 1,307 MB, 22.6% of nushell's heap after its base build, about 64M edges, a
  third of them on memos of dependencies (lean-frontend.md, **read**). `NEVER_CHANGE` inputs would
  drop about 250 MB of those (not run). The image's gain over eviction is largest here, and it is
  **unverified** until a dependency memo is really replaced.

### Per structure

Effort is S (days), M, L (a week or more), XL; gain is of the tracedecay worker's 7,878 MB (§1) and
is an estimate (**unverified**) unless a measurement is named. "Dependency share" is §1's
  proposed census where a structure is counted by files and crates, and is **not measured** here.

| structure | position-independent today | what blocks reading it in place | layout | gain, tracedecay | effort |
|---|---|---|---|---|---|
| file texts (450 MB) | yes: it is the file | `FileText.text` is an `Arc<str>` (`base-db/src/lib.rs:385`); 24 `.text(db)` sites in the subtree. Needs a text type that derefs to `str` over a mapping or an `Arc` | **none: map the registry file itself**; the kernel already caches it, it is clean and shared by every worker of the machine. Valid UTF-8 is checked once | most of 450 MB (the dependencies' share, §1) | S |
| symbol table (1,198 MB) | yes: strings and `u64` hashes | `SymbolTable` is `FxHashMap<String, Vec<Placed>>` and others, built while object files are read (`symbols.rs`) | a sorted array of path hashes to entries, one string blob; keyed by the content of each rlib | 1.2 GB to about 0.1 to 0.2 GB mapped; and the 6.5 to 12 s that loading takes | M |
| file parses (1,547 MB) | yes: flat arrays | the rowan fork's `flat.rs` owns boxed arrays; equality of parses compares them | the arrays and the text, 14 B an element, about 7 times the source (flat-syntax.md: 7,169 MB for 962 MB of source) | up to 1.4 GB, but see below: **eviction may beat mapping** | M |
| macro expansions (2,029 MB) | a tree: yes. Its spans: no | `Span` carries a `SyntaxContext` and an anchor file, both salsa-interned (`SyntaxContextImpl` 85 MB alone) | flat tree, span-map arrays, a table of contexts and files per section (the frozen format already names contexts once, by index) | the dependencies' share | L |
| bodies, signatures, item trees (1,985 MB) | no | `ExpressionStore` holds `Interned<TypeRef>`, `Path`, `Name`, `MacroCallId`, `BlockId`; signatures hold the same | an archive with out-of-line tables of strings, paths and types, and a translation per section. Any change to a `hir-def` type changes the format | the dependencies' share | XL |
| name resolution (669 MB) | yes (frozen) | the stream has no index; `ItemScope` is hash maps of `Name` to `PerNs` of salsa ids | per-module blocks and an index; decode a module on demand, evict | up to 0.6 GB | M to L |
| salsa records of dependency memos (nushell: about 430 MB) | n/a | disappear only if the memo is not made | read through an input or a hook that returns a view | see above | follows from the rows above |

Three of these are not about salsa at all, and are the cheapest: file texts, the symbol table and
(if mapping parses is chosen) flat trees. The rest need a translation table per section and a format
that moves with `hir-def`'s types: an image is valid for one build of ra-jit (the prototype's header
names it, and refuses another's), so every upgrade makes images again.

### Mapping or evicting

A part whose mapping and validation cost more than rebuilding it stays rebuilt (roadmap.md,
section 5). Parses are the case: lexing and parsing run at 40 MB/s on one core (flat-syntax.md,
24.1 s for 962 MB), the flat trees are 7 times the text, and the parse is read about once, to build
the item tree and the def map. A mapped parse spends disk and write time on something a worker
reads once; an evicted one costs a parse when it is read again. What makes the parse memo
stay is salsa evicting at a revision's start only (LOADMEM.md section 8; the patch that evicts
mid-revision is in `docs/upstream/daemon-memory/`, not applied). Which of the two is the unit's
to measure; expansions are the same question, with a larger cost to rebuild (a proc macro).

### Safety of a mapping

- The mapping is `PROT_READ`, `MAP_SHARED` of a file opened read-only. Asking the kernel for write
  access to it fails with `EACCES` (a test in the prototype; the mapping's flags, not isolation against
  job code).
- A file truncated under a live mapping faults (`SIGBUS`) on the first read of what is gone. The
  image is replaced by renaming another over it and never written in place, so a worker that mapped the
  old file keeps it.
- The root covers the index, and the index holds each part's SHA-256: checking the root reads a few tens
  of KB and no payload. Serving a part checks its SHA-256 before handing it to the decoder;
  a damaged part is read from the existing store instead. `RA_JIT_FRONT_IMAGE_VERIFY=1`
  eagerly checks every part. This is corruption detection, not a new trust framework.
  Writers must treat published inodes as immutable; the format cannot stop an external
  process from truncating or modifying one.

## 3. The prototype

`RA_JIT_FRONT_IMAGE=/absolute/path/front.image` opts in; unset, `0`, or `off` leaves it off.
`RA_JIT_FROZEN=off` also disables the image. The writer gathers available dependency parts
from the existing store after resolution, sorts and deduplicates them, and publishes a
single file by rename. The header names the blaze build, version and index root. The index
contains `(part-key, part-kind, offset, length, SHA-256)`; payloads are 64-byte aligned.
The map is read-only and shared. `Frozen.bytes` keeps the map alive with an `Arc` and an
offset range. There is no language-rule change and no new rustc/cargo path.

The image may cover a subset of a worker's dependencies. Missing, unreadable or corrupted
parts use the existing store; new parts are not appended to a mapped image. Rebuild or
remove the image to refresh it. Writing currently gathers all encoded parts into memory:
this is a bounded prototype design limitation, not the full streaming implementation.

The measurement driver is `harness/dist/frontimage.py`. It seeds once and interleaves
off/store/image arms, three runs per arm. A measurement-only hold at `front-end ready`
prevents codegen from racing the footprint sample. `memory::report` writes the ingredient
table at resolve when `RA_JIT_MEMORY` is enabled. Readiness is load + resolve + frozen
serving/writing/reporting; the individual timers are not the whole readiness time.
OS filesystem caches are not purged: “off” means cold semantic state, not cold disk.

### Clean-page pressure mechanism (read: predecessor's completed experiment)

Linux in Docker Desktop, Python standard-library holder, a 1,024 MiB random file and
300 MiB anonymous state, no swap, cgroup limit 1,444 MiB. A second process requested
1,084 MiB (rounded up to 1,088 by 16 MiB allocation chunks). Each pair gets a fresh
container. The limiter is the cgroup, not host memory pressure.

| arm | n | survived | holder RssAnon MiB, range | RssFile before, range | RssFile during pressure, range | reread seconds, mean (min–max) |
|---|---|---|---|---|---|---|
| heap | 3 | 0/3 (exit 137) | 1,336–1,336 | 6–6 | unavailable: holder killed | unavailable |
| map | 3 | 3/3 | 312–312 | 1,022–1,030 | 32–34 | 0.564 (0.494–0.651) |

The mapped runs kept their anonymous state and dropped about 1 GiB of clean file pages.
A separate sharing run (**n=1**, no spread; not a repeated result) used two holders:
heap cgroup usage 2,247 MiB, map 1,091 MiB; mapped Pss_File 513 MiB per holder, versus
heap Pss_Anon 1,031 MiB per holder. These are synthetic bytes and Python overhead.
They do not measure decoded frozen payloads, tracedecay, a Mac worker's footprint,
time to ready, or a speedup. Raw predecessor evidence is retained under
`blaze-targets/d3m/pressure/synthetic-1024.jsonl` in the coordinator evidence store.

## 4. The plan for the unit

1. Measure ownership at ready and build peak; attribute dependency/workspace bytes by
   ingredient rather than dividing crate counts. Keep same-session off/store/image baselines.
2. Stream the frozen-part image writer. Benchmark encoded-byte savings and decoding cost;
   retain this slice only if it pays for its index, hashing and publication.
3. Map immutable UTF-8 source and compact symbol arrays. Historical upper bounds are
   450 + 1,198 = 1,648 MB sampled, not a promised saving. The dependency fraction is unknown.
4. Compare mapped flat parses with eviction/reparse. Historical parse upper bound is
   1,547 MB; zero-copy arrays need rowan owner support. Measure write/read/rebuild time.
5. Add addressable def-map and expansion sections with image-index-to-salsa-id translation.
   Historical upper bound 669 + 2,029 = 2,698 MB; decoded caches still consume memory.
6. Archive bodies/signatures/item trees last (upper bound 1,985 MB). Validate actual edits,
   restarts and differential outputs; integrate soft-budget adaptation and host-pressure
   pausing, not a hard worker cap. No estimate here proves a 16 GiB tracedecay build fits.

## Not checked

Full zero-copy dependency queries, proc-macro state, syntax owner changes, Linux Rust
integration, tracedecay-scale pressure and restart behavior remain outside this prototype.
No fixture measurement or speedup is claimed until the guarded driver completes.
The predecessor's four image tests passed only in a stub crate; that was not integration
validation. The resumed tests add corruption fallback, duplicate keys and old-map lifetime.

## Deviations

The lock queue prevented the predecessor's full build and fixture measurements before its
provider failed. Synthetic container pressure is narrower than the requested real-project
pressure experiment. No new host-pressure controller or effect classifier is introduced.
The image retains decoded dependency data; it is level 1, not the full #274 daemon image.

## Commands

Prepared fixtures: ripgrep 15.1.0 and nushell 0.108.0 under
`/Users/bytedance/code/blaze-targets/d3m/fixtures`; no tracedecay edits.

```sh
# Every cargo build/test is inside this lock and the D3m tree/swap watchdog.
/Users/bytedance/code/blaze-targets/with-heavy-lock.sh d3m-sol-resume \
  python3 /Users/bytedance/code/blaze-targets/d3m/guard-sol-resume.py \
  bash /Users/bytedance/code/blaze-targets/d3m/check-sol-resume.sh
python3 -I harness/dist/test_frontimage.py
python3 -I harness/dist/frontimage.py --help
python3 -I harness/dist/frontimage_pressure.py --help
```

The full build script uses `CARGO_TARGET_DIR=/Users/bytedance/code/blaze-targets/shared`,
copies binaries to `d3m/bin/sol-resume`, then runs release image and frozen tests.
For real-project runs, hold the heavy lock around `frontimage.py`; it runs every blaze
client/daemon under `loadmem.py`'s footprint/swap watchdog and stops them between arms.
For container pressure, hold `with-docker-lock.sh`; the driver rejects a container over
6 GiB, uses equal memory/swap limits (no swap), and removes each container on exit.

## PR review follow-up

Publication now uses a unique temporary inode for each writer and removes it on error.
The former PID-only name let concurrent writes truncate one another and publish incomplete
images. The concurrency regression reproduced both a missing temporary file and an invalid
published image before the fix; it passes with independent temporary files. A second
regression checks cleanup when publication fails.

The measurement driver now requires actual mapped parts in the image arm and actual frozen
store reads in the store arm. Missing readiness samples, a failed initial build, an empty
seed and failed profiling arms are errors. It resets the relevant inherited configuration
and refuses reused run directories so stale logs cannot count as a new readiness event.
Cleanup addresses the private daemon, while the watchdog tracks only that client's daemon
and descendants. A readiness marker cannot override a simultaneous watchdog violation.
The cargo-home copy also works without macOS's `cp -c`.

Validation on Linux x86-64 with Rust 1.99.0: all nine tests in the actual `front_image.rs`
passed in a small test crate using the real mmap/hash/tempfile dependencies and the source
`Bytes` owner; store build identity and memory-reporting interfaces were stubbed. The two
new publication tests both failed against the old writer. The eight Python driver tests,
the driver help commands, touched Rust formatting and `git diff --check` passed. These are
focused checks, not full blaze integration, fixture measurements or a new speedup result.
