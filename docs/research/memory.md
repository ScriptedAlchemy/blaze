# Does the warm daemon leak, and what would shrink it

2026-10-26, for the distributed program (unit u4-memory): warm daemons ("slots") on fleet
machines, where a slot's memory decides how many fit on a 128-core/256 GB Linux box, a 64 GB M1
Ultra, a 12-core/64 GB box and a 12-core/16 GB laptop. Read at `c8e5f6a88a` (origin/dist/main,
which is upstream `indexable-inc/blaze` main). Nothing was built or run for this page: it is
the in-tree sources and research, upstream's issues and pull requests (read with `gh`, never
commented on) and the commits of every fork (read with `gh api`). Each claim is **read** (measured
by someone and recorded, or read in a source), **reported** (someone says so in an issue or a
pull request) or **unverified** (our inference). What would settle each open point is a proposed
measurement in [section 5](#5-proposed-measurements), not a number. This page adds to
[daemon-memory.md](daemon-memory.md) (what the heap is, salsa's `lru`, the allocator, sharing
debug types) and [lean-frontend.md](lean-frontend.md) (the heap by holder, interned arguments);
it does not repeat their tables.

## Short version

- **No true per-edit leak is shown.** The daemon between edits is flat on six small projects
  after 35 to 65 builds (ripgrep 1.6 GB ready, 1.6 GB after 65; fd 1.6 and 1.4; axum 2.0 and
  1.8), and on nushell, helix and bun the worker holds less after its edits than right after
  its own base (7.87 to 7.36 GB on nushell) (**read**, [benchmarks.md](../benchmarks.md#how-much-memory-and-disk-it-takes),
  [#197](https://github.com/indexable-inc/blaze/pull/197)). Nothing has run for thousands of
  edits: the longest recorded soak is 345 steps, and its author wrote that its slopes say nothing yet
  ([SOAK.md](../reports/SOAK.md)) (**reported**).
- **#261's jump (8.9 to 23.3 GB over two edits) is unexplained.** It is most like a build in
  full, which peaks at 23 to 27 GB on biome ([#242](https://github.com/indexable-inc/blaze/issues/242)),
  or an allocator that keeps its high-water mark, not a leak (**unverified**: the report has no
  worker log).
- **Retention without a bound exists, and grows with distinct things, not with edits.** The
  type interner of rust-analyzer is swept by `hir::collect_ty_garbage()`, which rust-analyzer
  calls whenever it is idle after a revision; nothing in `crates/` calls it (**read**).
  Interned arguments of four hot queries are kept for good (#253). A worker never unloads a
  checkout (**read**). None of it is measured over a long run (**unverified** as a size).
- **The cheapest bound on everything is a recycle:** stop a worker that grew well past its
  ready size while it is idle and load it again from the store. The daemon already stops idle
  workers after 30 minutes and, when over its ceiling, the one asked longest ago; neither looks
  at growth (**read**).
- **Top five** ([section 4](#4-ranked-improvements-still-to-make)): (1) reclaim at idle in
  `let_go`: type GC, solver caches on every thread, arena purge; (2) recycle on growth; (3) one
  worktree path per slot, and unload the checkouts that are gone; (4) bound a cold build's
  peak (#203: hyperfine 2.5 to 9.8 GB), which is what a 16 GB laptop hits first; (5) share the
  standard library and dependency layer between slots (#40, #274, #283), which is 0.9 to
  1.3 GB per worker today.
- **Slot sizes, Linux x86-64, 16 cores pinned** (**read**): small projects (ripgrep to axum)
  1.4 to 2.0 GB; helix 5.7 GB, nushell 7.1 GB; bevy 15.2, biome 19.3, bun 24.5 GB; an idle
  daemon with one tiny project 1.28 GB. Reported, not measured by us: Tempo 10.1 GB loaded,
  reth 19.6 GB loaded ([#261](https://github.com/indexable-inc/blaze/issues/261)). tracedecay's
  own size is not known: u1 is building it, and the first measurement of section 5 is its.

## What was and was not done

- **Read:** `docs/`, `harness/`, `crates/ra-jit`, `crates/hir-ty-codegen`, the vendored salsa,
  `intern` and the memory-relevant parts of rust-analyzer; with their comments, the issues and
  pull requests the brief named (#29, #39, #60, #188, #203, #242, #259, #261, #274; #146, #163,
  #197, #198, #225, #229, #240, #253, #283) and #4, #24 and #221 from the searches; the keyword
  searches (memory, RSS, OOM, evict, leak, jemalloc, mimalloc, LRU, arena, intern,
  shrink, footprint, cgroup) over upstream's issues and pull requests; the branches of all
  18 forks, compared with upstream main by GraphQL, commit messages (headline and body) searched
  for the same words. One fork commit's diff was read (`cd1caf0141`); the others by message.
- **Not done:** no build, no run, no profile. Every size here is somebody else's, with their
  host. The published memory numbers are all pinned to 16 cores (`taskset`) on a 96-core or
  64-core host, so what a 128-core unpinned host does to the allocator is unknown
  (**read**, `docs/benchmarks/linux-x86-64.json` `host`).
- **macOS is old.** The macOS rows of benchmarks.md are from `84760c3f` (2026-10-21) on an M5
  Max with other work running, before jemalloc (#146) and before the debug types were shared
  (#163, #197). They say nothing about an M1 Ultra or a 16 GB laptop today (**read**,
  `docs/benchmarks/macos-arm64.json`).

## 1. A leak, or memory held by design

### 1.1 What the growth numbers say

| Evidence | What it shows | Label |
|---|---|---|
| benchmarks.md, Linux x86-64, `1a005421`, "daemon between edits" | ripgrep 1.6 GB ready, 1.6 after 65 builds; fd 1.6, 1.4 (55); bat 1.9, 2.0 (55); hyperfine 1.8, 1.5 (55); just 1.8, 1.9 (55); axum 2.0, 1.8 (35): flat | **read** |
| same table, big projects | nushell 5.2 to 7.1 GB (65 builds), helix 4.2 to 5.7 (55), bevy 8.2 to 15.2 (35), biome 6.9 to 19.3 (57), bun 10.5 to 24.5 (33). "Ready" is before the own base, which the daemon builds one second after, so these steps are mostly the base | **read** |
| daemon-memory.md, "Why the daemon doubles after ready" | the base adds 3.9 GB on nushell (4.1 to 8.0 GB live) and 6.8 GB on helix (3.2 to 10.0), of which `described` was 59% and 82%; that was before #163 and #197 | **read** |
| #197's table (main to branch, `memsplit.py`, nushell) | the worker after the base holds 8.97 GB (main) and 7.87 GB (branch), and after its edits 7.63 and 7.36 GB: edits lower it. The same for helix (7.02 to 6.55 and 6.14 to 5.87) and bun (18.77 to 15.97 and 15.85 to 14.47) | **read** |
| SOAK.md, 783 steps in 4.4 h on an 8-core 31 GB host, an older branch | slopes of resident memory +815 MB and +1,830 MB per 1,000 steps in two runs, which the author reads as "where the run happened to end": the totals follow how many workers are alive. The daemon process alone went 15 to 12 MB | **reported** |
| SOAK.md, same | "A run of thousands of steps is needed before a slope means anything, and has not been done." | **reported** |
| #261 | worker 10,053 MB ready, 8,863 MB after 7 daemon builds, 23,261 MB after 2 more edits (9 builds) | **reported** |
| `benchhistory.py` | records `resident_mb` after each phase and `resident_end_mb`, and fails a run on a move past its noise band (floor 100 MB), so a regression of the end-of-run figure is gated | **read**, `harness/benchhistory.py:55-59` |

The ceilings are not a growth test: `soak.py` prints a slope per 1,000 steps
(`harness/soak.py:943-971`) but asserts nothing about it, and `soakrows.py` has two memory rows
(`memory ceiling`, `no memory ceiling`) that check which side builds, not how much is held
(`harness/soakrows.py:1091-1092`) (**read**). `worktree deleted and recreated` records a status
and checks the build, not memory (`harness/soakrows.py:272-297`) (**read**).

### 1.2 What #261 can and cannot be

The report is one run of `cargo ra-jit status`, which prints what each worker holds at that
moment: `held_by(pid)`, a resident size on Linux (`crates/ra-jit/src/cargo/daemon.rs:748-752`,
`crates/ra-jit/src/memory.rs:28-30`) (**read**). Three readings fit it, none proven:

1. **A build in full, and the own base after it.** An edit that cannot be a patch makes the
   worker build the whole program; biome's did so for 232 s at a 27 GB peak after a first edit
   ([#242](https://github.com/indexable-inc/blaze/issues/242), comment 2026-10-25T02:45) and for
   136.5 s at a 23 GB peak on a signature edit (same issue, comment 09:08) (**reported**). When a build in full finishes, the
   daemon builds its own base again once nothing is asked (`build_own_base`,
   `crates/ra-jit/src/main.rs:9347`, `OWN_BASE_AFTER` 1 s at `:6529`). Tempo's `main.rs` edit
   adds an `eprintln!` to `main`; whether it made a build in full is not in the report
   (**unverified**).
2. **An allocator that has not given pages back.** A build's peak is far above what stays: on
   nushell the worker's high-water mark is 13.2 to 13.4 GB against 9.5 GB held after
   (daemon-memory.md, "The allocator") (**read**). jemalloc keeps freed pages for ten seconds
   to reuse (`crates/ra-jit/src/memory.rs:128-129`); the daemon calls `give_back` (an arena purge)
   only when it is over its own-base budget (`memory.rs:128-144`, `main.rs:9035-9037`) and sets
   no `malloc_conf` and no background thread (`grep` of `crates/`, `Cargo.toml`, `.cargo`)
   (**read**). That jemalloc purges decayed pages only when an arena is used, or from a
   background thread, is from its documentation and not checked in this tree; so whether an
   idle worker keeps its high-water mark is **unverified**.
3. **A real leak.** Possible; the holders that could grow with edits are in 1.3. If it were
   one, 14 GB over two edits would be 7 GB an edit, far above anything the soak or the
   benchmark's 65 builds show (**unverified**).

The report also says reth is 19.6 GB right after loading, with 904 crates and 7,521 proc macros
expanded in 43.5 s ([#259](https://github.com/indexable-inc/blaze/issues/259)) (**reported**).
That is load, not edits: a heap profile with `heap.py` (it names the holder by salsa query)
would say whether it is macro expansion, as `parse_macro_expansion` and `parse` are 0.8 GB of
nushell's 10 GB (daemon-memory.md) (**unverified**).

### 1.3 Who holds memory after an edit

What an edit makes, and what keeps it. "Grows with" is what the size follows. Every row is
**read** in the source unless it says otherwise; sizes are in the last column when someone
measured them.

| Holder | Where | Grows with | Freed by | Size, when measured |
|---|---|---|---|---|
| Replaced memos of a revision | salsa's `deleted_entries`, `vendor/salsa/src/function.rs:318-352` | edits in one revision | the next revision, or `trigger_lru_eviction` (`zalsa.rs:469-476`); `let_go` calls it after each build (`main.rs:6415-6420`) unless a build or a tier compile is running | the own base's one long revision holds all of them (#188) |
| Memos of every query that has no `lru` | `infer`, `mir_body`, `runtime_mir_body`, layouts, `inlinable`, `compiled_instance`, `told_type`, `compiled_root` (no `lru` in `crates/hir-ty-codegen` or `vendor/rust-analyzer/crates/hir-ty`) | distinct bodies and instances of the loaded crates | an edit that changes the memo, or the end of the worker | after the base on nushell: `described` 2.44 GB then, `runtime_mir_body` 0.55, `compiled_instance` 0.44 + 0.21, `mir_body` 0.37, `infer` 0.24 (daemon-memory.md) |
| Salsa's records (edges, revisions) | per memo | memos | their memo | 1,307 MB, 22.6% of 5.8 GB sampled on nushell; about 64M edges, a third on memos of dependencies (lean-frontend.md) |
| Interned arguments kept for good | `row_place`, `source_place`, `compiled_instance`, `instance_serial` take `revisions = usize::MAX` (`crates/hir-ty-codegen/places.rs:198,211`, `mod.rs:2491,2840`) | **every distinct instance ever asked**, over the worker's life | nothing | not measured; #253's text says "never freed in a long session" for the idea (lean-frontend.md:183). `compiled_instance` has 158,496 memos on nushell, 1.91M on zed |
| Types, generic arguments, clauses | the `intern` crate's tables, swept only by `hir::collect_ty_garbage()` (`vendor/rust-analyzer/crates/hir-ty/src/next_solver/interner.rs:2696`, `intern/src/gc.rs:110`) | distinct types ever built, including those of memos since replaced | nothing in blaze: no caller in `crates/`. rust-analyzer calls it from its main loop whenever idle after a revision (`vendor/rust-analyzer/crates/rust-analyzer/src/main_loop.rs:578-587`, `ide/src/lib.rs:216-224`) | not measured. lean-frontend.md counts "interned values" at 411 MB, 7.1% of the heap after the base (a row that may include salsa's own interned values), and says the `intern` values are "never collected" (`:342-344`) |
| Trait-solver caches, per thread | `thread_local! GLOBAL_CACHE`, up to `KEPT = 32` per thread, reset only when that thread next solves at a new revision (`interner.rs:2596,2628,2670`) | threads that solved once, then went idle (rayon's pool is the machine's cores) | a new revision, on that thread only; `clear_tls_solver_cache()` clears the caller's | not measured (**unverified**) |
| Checkouts | `Checkouts::add` appends to `roots`, `splits`, `debug` and `loaded`; there is no removal (`main.rs:2307-2330`, `:2542-2610`; `reload` replaces in place, `:2648-2690`) | every worktree path a worker was given | the end of the worker | not measured. A second checkout alike the first is a view that holds only the packages that differ (`main.rs:2542-2560`) |
| File hashes | `FileHashes` keeps each file's last text and hash "for as long as the daemon runs", by `FileId` (`crates/ra-jit/src/content.rs:330-355`) | files ever hashed; the old text of a file edited since | the end of the worker | not measured; texts only, tens of KB each |
| rustc's symbols | `RUSTC_SYMBOLS` puts the read `Linked` back for each binary path and never removes it (`main.rs:4754-4790`) | executables a worker patched | the end of the worker | "symbol table" is 229 MB on nushell and 1.9 GB on zed, of which this is part (daemon-memory.md, #188) (**unverified** which part) |
| The patch base | `Rejections` (`hir-ty-codegen/mod.rs:1284`), `Frozen` (`patch.rs:592`), `Baseline.originals` (`splice.rs:126`); `spliced` is cut to `KEPT` (`splice.rs:484-486`) | crates and files edited, addresses patched | `start_again` resets `Rejections` after an interrupted or panicked build (`main.rs:6679`, `:9360`); nothing prunes it between | small by construction (sets of crates and files, bytes per patched address) (**unverified**) |
| Salsa's table pages | `Table::push_page` only; no page is ever removed (`vendor/salsa/src/table.rs:309-314`) | the most slots alive at once | the end of the worker | the high-water mark of the revision the base is built in (**unverified** as a size) |
| Allocator | jemalloc, no `malloc_conf`, purge only over budget | the high-water mark of a build | decay on allocator activity | nushell: 13.2 to 13.4 GB high-water against 9.5 GB held (daemon-memory.md) |
| Proc-macro server | a process beside the worker | expansions | the end of the daemon | 85 MB on ripgrep-sized projects (daemon-memory.md); reth's 7,521 expansions are not measured (**unverified**) |
| Store | mmapped packs (file-backed), unpacked-block cache 32 MiB (`crates/ra-jit-store/src/lib.rs:80`) | the store | `RA_JIT_CACHE_MAX` | file-backed pages count in `VmRSS` (`memory.rs:28-30`), so the ceiling counts memory the kernel can drop (**unverified** as a size; `memsplit.py` reports `RssFile` apart) |

One more holder belongs to the next architecture: blaze-next's Merkle VFS keeps every node ever
made until `Tree::collect(live)` is called, and the daemon's `main.rs` and `tree.rs` on
`blaze-next` do not call it (a three-file transaction appends about 233 KB; switching a branch
that touches the whole tree adds the tree) (**read** in `crates/blaze-vfs/src/lib.rs` and the
two files on `blaze-next`; **unverified** that no other file calls it).

### 1.4 Verdict

- **Leak:** none shown, none excluded. The evidence is 65 builds and a 345-step soak, and
  nothing reaches days of PR edits.
- **By design, without a bound:** the type interner (never swept), immortal interned arguments,
  checkouts, and the allocator's high-water mark. Each grows with distinct things a slot sees,
  which over days of PR heads is more than an edit count suggests (**unverified**).
- **Bounded:** see section 2.

## 2. What is bounded and what is not

### Bounded

| What | Bound | Where | Label |
|---|---|---|---|
| All workers together | `RA_JIT_MEMORY_MAX`: half of the machine, or of the control group when that is less (#229); a size or `off` | `daemon.rs:46-60`, `memory.rs:44-90` | **read** |
| Which worker goes when over it | the one asked longest ago; never a busy one, never the one asked last. Checked once a second, not while a live-edited program runs | `daemon.rs:96-112`, `:1336-1347`, `:1389-1406` | **read** |
| An idle worker | stopped after 30 minutes without a request (`RA_JIT_IDLE_SECS`) | `daemon.rs:35`, `:1348-1372` | **read** |
| The own base | `RA_JIT_OWN_BASE_MEMORY`: a quarter of the machine or control group; it purges, then gives the base up and edits patch rustc's executable (#159). Applies only while the base builds | `main.rs:6532-6543`, `:9030-9050` | **read** |
| A spare worker | none held: one starts with each build of a new workspace (#56); `RA_JIT_SPARE=ready` brings back the 1.3 GB one | `daemon.rs:64-88` | **read** |
| Parse trees and bodies | `lru`: `parse` 128, `parse_macro_expansion` 512, `ast_id_map` 1024, `with_source_map` 512, `Body::relative` 4096, attributes 50 to 250 (`base-db/src/editioned_file_id.rs:22`, `hir-expand/src/lib.rs:494,1799`, `hir-def/src/expr_store/body.rs:94,144`, `hir-def/src/attrs.rs:1228-1313`). They evict at a new revision or at `let_go`, not during a build | `vendor/salsa/src/function.rs:505-515` | **read** |
| Interned values of ordinary queries | collected 3 revisions after their last use | `vendor/salsa/src/interned.rs:54` | **read** |
| Detail spans, solver caches per thread | 4,000 spans between builds; 32 caches a thread | `trace.rs:150-157`, `interner.rs:2596` | **read** |
| Daemon process | 15 to 28 MB in 783 soak steps | SOAK.md | **reported** |

### Not bounded

| What | Why | Label |
|---|---|---|
| One worker over the ceiling, alone | `over()` keeps the one asked last; `RA_JIT_MEMORY_MAX` does not shed inside a worker | **read**, `daemon.rs:100-103` |
| A worker after its base is built | the own-base bound stops at the end of the build; nothing checks the worker's size afterwards but the daemon's ceiling above | **read**, `main.rs:6531-6534` |
| Memos of `infer`, MIR, layouts, codegen queries | no `lru`; the runtime capacities are commented out (`vendor/rust-analyzer/crates/ide-db/src/lib.rs:364-372`) and `RootDatabase::new(None)` is what blaze calls (`main.rs:2295`) | **read** |
| `lru` on a query with cycle handling | vendored salsa still throws `Cancelled::PropagatedPanic` for an evicted memo (`execute.rs:365-388`); the 13-line fix is a saved patch, not applied (`docs/upstream/daemon-memory/salsa-0.28.5-evicted-memo.patch.txt`) | **read** |
| Anything during one revision | an `lru` evicts only at a new revision, so the base's peak keeps everything (#188) | **read** |
| Type interner, interned arguments, checkouts | section 1.3 | **read** |
| Memory read as `VmRSS` | file-backed and shared pages count (Linux); macOS uses the physical footprint, which counts compressed pages | **read**, `memory.rs:28-42` |
| A cold build's processes | build scripts compile as separate ra-jit processes each with the standard library's maps: ripgrep 2.6 to 4.6 GB, hyperfine 2.5 to 9.8 GB since #186 (#203) | **reported** |

## 3. Every improvement found

Effects are the authors', on their hosts; the first column is the number to open.

### Merged

| # | What | Effect | Label |
|---|---|---|---|
| [#146](https://github.com/indexable-inc/blaze/pull/146) | jemalloc on Unix (mimalloc kept for Windows) | after edits ripgrep 2.03 to 1.57 GB, nushell 10.80 to 9.52, helix 12.13 to 11.07, bun 27.00 to 24.28; mimalloc held 2.5 to 2.9 GB more than jemalloc on nushell on a host with THP on `madvise` | **read** (daemon-memory.md) |
| [#163](https://github.com/indexable-inc/blaze/pull/163) | debug type sets share what each type is told as (#55); the own base purges before giving up | after edits nushell 9.74 to 7.46, helix 11.30 to 6.39, bun 24.87 to 15.94; most at an edit 13.96 to 11.73, 14.54 to 9.89, 33.62 to 23.23 GB | **read** |
| [#197](https://github.com/indexable-inc/blaze/pull/197) | each debug type described once, in strongly connected groups (#29) | after edits nushell 7.63 to 7.36, helix 6.55 to 5.87, bun 15.97 to 14.47; zed's `described` 31 GB of a 47.8 GB sample to 2.4 GB; zed's base finishes at a 62.6 GB peak, 51.5 GB held | **read** |
| [#198](https://github.com/indexable-inc/blaze/pull/198) | MIR ways out share drops as rustc's drop trees do | Xlib's `open` 2,986,013 to 29,201 blocks; bevy `--crate breakout --dev-deps` 364 GB OOM to 76 GB peak in 516 s | **reported** (the pull request's title) |
| [#225](https://github.com/indexable-inc/blaze/pull/225) (dyc3's patch, finished by andrewgazelka) | types of variables worked out only when variables are written | biome cold start peak 40.4 to 30.6 GB (32 cores, 40 GB cap), `told_type` memos 726,516 to 0; at 16 cores on main with #250 the worker's base 25.0 to 24.3 GB, after edits 19.0 to 18.5 GB | **reported** |
| [#229](https://github.com/indexable-inc/blaze/pull/229) | memory bounds follow the control group; biome's first edit stays the daemon's | a daemon in a 6 GB scope said "workers hold at most 96,021 MB", now 3,072 MB; the avoided rebuild was 130 to 290 s and up to 27 GB | **reported** |
| [#250](https://github.com/indexable-inc/blaze/pull/250) | a source a build script touches is no change | biome's first edit 232 s and 27 GB peak becomes 0.7 s | **reported** |
| e7d25d2801 (#36, #39) | `RA_JIT_MEMORY` reports after each request, `heap.py`, `heapfold.py`, the footprint on macOS | what holds memory, by salsa ingredient (daemon-memory.md); no memory saved by itself | **read** |
| 8e01def1cf (#56) | the spare worker is started with a build of a new workspace, not held | after edits ripgrep 3.63 to 2.31 GB, fd 3.42 to 2.04, nushell 12.70 to 11.36; idle daemon 2.64 to 1.28 GB | **read** |
| 6d735e34ce (#29) | spell each interned type once per daemon in stable names | helix cold: peak 100.7 to 18.4 GB, store 14 to 1.6 GB | **reported** ([#29](https://github.com/indexable-inc/blaze/issues/29)) |
| [#253](https://github.com/indexable-inc/blaze/pull/253) | interned arguments of four queries kept for good | time, not memory; a memory risk named in 1.3 (the earlier fork measured −39 MB of salsa metadata) | **read** |
| [#221](https://github.com/indexable-inc/blaze/pull/221) | the Linux x86-64 bench run again at `42cb85270f` (after #163 and #197), the standard library's unit fetched as a release does | the daemon after edits: helix 12.3 to 6.0 GB, bevy 39.6 to 15.0, nushell 10.9 to 7.2 | **reported** |

### Open pull requests and issues (upstream)

| # | What | State | Label |
|---|---|---|---|
| [#283](https://github.com/indexable-inc/blaze/pull/283) | blaze-next: Merkle VFS, push invalidation, sans-IO core, agent API. Its body has no memory number. `docs/roadmap.md` states budgets: salsa's records 1.3 GB to under 0.3 GB through an item-level graph, "ra-jit's own" 988 MB halved, a second head under 10% of the first | draft; targets **unverified** | **read** |
| [#274](https://github.com/indexable-inc/blaze/issues/274) | state as an image: mmapped, so a restart or an ix fork starts where the last stopped; target a warm restart under 1 s (35.4 s on nushell) | open | **reported** |
| [#188](https://github.com/indexable-inc/blaze/issues/188) | build the own base in pieces, a revision apart, so `lru` frees MIR, inference and layouts between them; needs `compiled_instance`'s code outside the database (#41). zed under the default bound (31.7 GB) is given up today | open | **reported** |
| [#40](https://github.com/indexable-inc/blaze/issues/40), [#41](https://github.com/indexable-inc/blaze/issues/41) | frozen dependency artifacts; a warm-start manifest. `NEVER_CHANGE` inputs would drop about 250 MB of salsa records on nushell (**unverified**, lean-frontend.md) | open | **reported** |
| [#203](https://github.com/indexable-inc/blaze/issues/203) | cold-build peak: script compiles run as ra-jit processes beside the daemon; send them to the daemon that holds the standard library, or bound how many run at once | open | **reported** |
| [#261](https://github.com/indexable-inc/blaze/issues/261) (gakonst) | Tempo 10.0 to 8.9 to 23.3 GB, reth 19.6 GB at load; asks for a breakdown and a way to shed | open, no reply | **reported** |
| [#259](https://github.com/indexable-inc/blaze/issues/259), [#242](https://github.com/indexable-inc/blaze/issues/242) | reth's first edit never finishes (~20 GB, 800% CPU); biome's builds in full at 23 to 27 GB | open | **reported** |
| [#105](https://github.com/indexable-inc/blaze/issues/105) | share a cache over worktrees | open | **reported** |
| [#60](https://github.com/indexable-inc/blaze/issues/60) | two soakrows (one the memory ceiling) failed on main | closed in 75 minutes | **reported** |
| [#24](https://github.com/indexable-inc/blaze/pull/24) | `RA_JIT_HYDRATE=0`; not memory, and declined as a knob | open | **reported** |

### Salsa and fork commits

| Commit | What | Label |
|---|---|---|
| [`cd1caf0141`](https://github.com/indexable-inc/blaze/commit/cd1caf014152eb3c1d13708bcd3b6ecb488f0335) on `blaze-next` (andrewgazelka; in the branches of ScriptedAlchemy, tobi and RandazzoMaxime) | vendored salsa: a memo whose value the LRU dropped is verified before it is computed again, and backdated when its inputs are the same (`function/backdate.rs`, `fetch.rs`, `execute.rs`, `specify.rs`, a 55-line test). It removes the cost that stopped #57: an evicted memo could not backdate, so a signature edit went from 0.42 s to 0.64 to 2.43 s | **read** (the diff); effect on edits **unverified** |
| [`a0b116443c`](https://github.com/indexable-inc/blaze/commit/a0b116443cb83ba8fb5be2517939ab63f809aec2), then [`05d1bee206`](https://github.com/indexable-inc/blaze/commit/05d1bee2066955f429277e7d768151469abb43ab) | files loaded at a durability of their own so a first edit does not rerun what the LRU dropped (2.6 to 3.8 s against 0.36 s on nushell's first body edit); the second is the same lane's follow-up. Tried, found unsound, taken out in #350 (PR #283's text); what stays is `cd1caf0141` | **reported** (commit messages, PR #283) |
| `docs/upstream/daemon-memory/salsa-0.28.5-evicted-memo.patch.txt` | 13-line salsa patch: an evicted memo is not a panicked cycle head; a provisional memo is not evicted. Not applied; the draft issue for salsa was not filed | **read** |

### Measured and dropped (do not redo as they were)

- `lru` on `infer`, `mir_body`, `runtime_mir_body` and `inlinable` with the patched salsa: nushell
  0.35 to 0.5 GB of 12.9 GB, with a signature edit from 0.42 to 0.64 to 2.43 s, one run
  ([#57](https://github.com/indexable-inc/blaze/issues/57), daemon-memory.md). That was with
  mimalloc, which returned nothing it was given: "a forced `mi_collect` after each request
  returns 30 to 40 MB". It is **unverified** how much more jemalloc plus an arena purge gets
  from the same eviction.
- Loading the library into the spare without resolving it, a spare that resolves on every core,
  and no spare at all (`RA_JIT_SPARE=0`): none beat starting one with each new build
  (daemon-memory.md, "The spare").
- jemalloc `narenas:16` (10.76 GB against 9.46 to 9.60 at the defaults, nushell);
  `background_thread:true,dirty_decay_ms:1000,muzzy_decay_ms:0` (9.50 GB, no gain);
  `dirty_decay_ms:0,muzzy_decay_ms:0` (8.98 GB, 0.5 to 0.6 GB less, cost to edits unmeasured).
- Store eviction by generations ([#4](https://github.com/indexable-inc/blaze/pull/4), closed,
  DO NOT MERGE) is disk, not memory.

## 4. Ranked improvements still to make

Ranked by how sure the benefit is for a slot that lives for days and takes many PR edits,
then by size over effort. Effects are **unverified** unless a measured number is named.

| # | Improvement | Expected effect | Effort | Risk | Touches |
|---|---|---|---|---|---|
| 1 | **Reclaim at idle, in `let_go`.** After a build, when no build and no tier compile runs: `hir::collect_ty_garbage()`; clear the solver cache on every rayon thread; `give_back()` (arena purge); `background_thread:true` so an idle worker decays. rust-analyzer does the first at every idle revision | the only never-swept global is the type interner, so a growth that follows distinct types goes; the purge is measured at 0.5 to 0.6 GB (6%) on nushell for `dirty_decay_ms:0`. Size of the rest unknown | S | the GC is `unsafe` and needs no unrecorded type values: holders of raw interned refs outside salsa must be checked; a purge faults pages back in at the next build; the vendored `ide` avoids `trigger_lru_eviction` as "bugged wrt to cancellation" (`ide/src/lib.rs:216-222`) and blaze calls it in `let_go` | `crates/ra-jit/src/main.rs:6415`, `memory.rs:128`, a call in `hir` already exported |
| 2 | **Recycle a worker that grew.** When a worker holds more than `k` times its ready size (or its last measured steady size) and is idle, `let_go` it and load it again from the store, as the idle timer does today at 30 minutes. Hysteresis, one recycle at a time per machine, never with a live-edited program running (`live::any_running` already guards the ceiling) | bounds growth from any cause, known or not (#261); the price is the warm start: ripgrep-class 1.3 to 2.6 s to ready (daemon-memory.md, "The spare"), nushell 35.4 s and helix 38.3 s from the store (#274; warm-start.md has the history), and the base is built again in the background | S to M | a recycled worker's first edit is slow (12 s at the client on nushell, lean-frontend.md); another slot must serve meanwhile, which is what the distributed design has | `crates/ra-jit/src/cargo/daemon.rs:1336-1406` (`fit`, idle loop), `held()` at `:748` |
| 3 | **One worktree path per slot, and unload the gone ones.** The coordinator moves one checkout between PR heads with file edits instead of a path per PR; `Checkouts` gets a removal so a worktree that is deleted is dropped from `roots`, `splits` and the crate graph | stops growth with PR count; saves one view per retired worktree, size unknown | S for the policy, M for the unload | unload must keep a base another checkout is a view of (`Split.base`, `main.rs:2542-2560`); the crate graph and `FileId`s are salsa inputs | coordinator policy (no blaze change); `main.rs:2307-2720`, `crates/ra-jit/src/cargo/daemon.rs:409-420` |
| 4 | **Bound a cold build's peak** (#203): send script compiles to the daemon that already holds the standard library, or cap how many run at once | the peak went ripgrep 2.6 to 4.6 GB, bat 4.6 to 7.9, hyperfine 2.5 to 9.8, just 3.7 to 8.7 since #186 (**reported**); back near the stored figure would let a 16 GB laptop take a slot | M | script compiles that wait on one process are slower; a sandboxed script must not share a process (#147) | `crates/ra-jit/src/cargo/own_script.rs`, `sandboxed.rs`, `daemon.rs` |
| 5 | **Share the standard library and dependency layer between slots** (#40 frozen dependencies, #274 the image, #283's item-level graph). Memory mapped read-only is shared by every worker of a machine | the standard library alone is 0.9 GB (940 MB on a Mac, spare, ripgrep) to 1.3 GB (Linux, idle daemon with a tiny project) per worker (**read**); the roadmap's budgets (salsa records 1.3 GB to under 0.3 GB on nushell, a second head under 10% of the first) are targets | L, in progress | the image must be byte-stable across versions; the item graph moves where invalidation lives | `vendor/rust-analyzer` (frozen parts), `crates/` (image, store), #283 |
| 6 | **Use anonymous resident memory for the ceiling and the base bound** (`RssAnon` on Linux) instead of `VmRSS` | stops evicting a worker for mapped store pages the kernel can drop; unknown size, `memsplit.py` has `RssFile` per step | S | under a cgroup the page cache is charged too, and `memory.max` can still kill; keep the cgroup's own number for the hard bound | `crates/ra-jit/src/memory.rs:28-30`, `daemon.rs:748-764`, `main.rs:9034` |
| 7 | **Drop what only the first edits need**: the rustc symbol table once the own base takes edits (`RUSTC_SYMBOLS`), `FileHashes` entries of files gone, `Rejections` maps a complete base no longer reads | part of the 229 MB (nushell) to 1.9 GB (zed) symbol table, and a few MB of texts | S | the splice reads rustc's names to find a missing symbol after a base is given up (#159) | `main.rs:4754-4790`, `content.rs:330-355`, `hir-ty-codegen/mod.rs:1284` |
| 8 | **`lru` on `infer`, MIR, layouts, `inlinable`** with the cycle patch and `cd1caf0141`, evicting at idle with an arena purge after | values on nushell are about 1.2 GB; with mimalloc 0.35 to 0.5 GB came back; the second salsa patch may remove the signature-edit cost | M | the first patch is on a vendored salsa that is "as published"; edits that reach an evicted body re-verify before they recompute (the cost to measure) | `vendor/salsa/src/function/*`, `lru =` attributes in `vendor/rust-analyzer/crates/hir-ty` |
| 9 | **Build the own base in pieces** (#188) with `compiled_instance`'s code kept outside the database | zed-class: a 62.6 GB peak under a 31.7 GB bound; small for nushell-class (about 1.2 GB of values at stake) | L | the base is one revision today; pieces need the code held in the store (#41) | `crates/ra-jit/src/main.rs` (own base), `hir-ty-codegen` |
| 10 | **Give the immortal interned arguments a ceiling.** `revisions = usize::MAX` on four queries (#253) becomes a large `N`, or the arguments are counted and a recycle (item 2) is asked at a count | stops a monotone table keyed by every instance ever asked; cost is the verification #253 removed (about 7% of a signature edit, 39 MB) | S once measured | an argument that holds a collected value must not outlive it (the reason #253 limited itself to four) | `crates/hir-ty-codegen/places.rs`, `mod.rs:2491,2840` |
| 11 | **Look at the proc-macro server's memory over many expansions** (reth: 7,521) and restart it past a size | unknown | S | a restart loses its state | `crates/ra-jit/src/cargo/own_macro.rs`, `vendor/proc-macro-srv` |
| 12 | **Call `Tree::collect` in blaze-next's daemon** at idle, with the heads and kept roots as live | bounds the VFS arena to what the heads reach | S | renumbers `NodeId`s: every holder of one must be rewritten | `crates/blaze-vfs`, `crates/ra-jit/src/tree.rs` on `blaze-next` |

Notes on the ranking.

- Item 1 is first because it is small, is where the one unswept structure is, and is the
  precedent rust-analyzer already has. Whether it matters is the first thing the soak of
  section 5 decides.
- Item 2 is second because it holds whatever the soak finds, and its cost (a warm start) is
  what #274 removes. A distributed design has other slots to serve while one recycles.
- Item 3 costs nothing in blaze if the coordinator does it, and it is the likeliest source of
  growth for a slot that takes one PR after another (**unverified**).
- Items 5 and 9 are large lanes that already have owners (#40, #274, #283, #188); this page
  only says what they would save a slot.
- For the 16 GB laptop the order is 4, then 2 and 6: a project the size of ripgrep is 1.6 to 2 GB
  at rest, and the cold build's peak, not the rest, is what needs room. The ceiling defaults to
  half of the memory, 8 GB, and the own base to 4 GB (`daemon.rs:46-60`, `main.rs:6532-6543`).

## 5. Proposed measurements

Each is a harness someone can write with what exists: `RA_JIT_MEMORY=<dir>` writes, after load,
resolve, build, warm-up and each request, one line per salsa ingredient (`count`, field bytes,
record bytes, heap bytes) and a jemalloc profile with `--features heap-profile` and
`_RJEM_MALLOC_CONF=prof:true` (`crates/ra-jit/src/memory.rs:146-224`); `heap.py` and
`heapfold.py` name the holders; `memsplit.py` splits a tree by process and by page kind
(`VmRSS`, `RssAnon`, `RssFile`, `VmHWM`); `leanprof.py` runs phases; `soak.py` keeps a
resident figure and a slope per 1,000 steps. `RA_JIT_MEMORY` slows edits to about 1.5 s, so
timings and memory come from separate runs (lean-frontend.md).

### M1. Edit/undo growth, with a per-holder breakdown (the main one)

`harness/memgrowth.py PROJECT --pairs N --reclaim-every K`:

1. Start a daemon with `RA_JIT_MEMORY`, a heap-profile build, and one worker; wait for ready
   and for `edit-to-binary:` of the own base (as `memsplit.py` does).
2. Apply `N` edit/undo pairs from a seeded list of kinds (body, signature, new function, new
   file, generic, a move of an item), each returning the text to its starting bytes. After
   each pair, wait for quiet.
3. Every `K` pairs record, with the solver cache cleared: `VmRSS`, `RssAnon`, `RssFile`,
   `VmHWM`; jemalloc `stats.allocated`, `stats.resident`, `stats.mapped`, `stats.retained`
   (today only `allocated` is read, `memory.rs:118-126`); the ingredient table; and counts the
   report does not have: the `intern` tables' lengths per type, `Checkouts.roots`,
   `FileHashes`, `RUSTC_SYMBOLS`, `Rejections` maps, `Baseline.originals`, thread-local solver
   cache sizes (a small `own` section of `memory::report`).
4. At the end, in this order, and read after each: the type GC, the arena purge, a worker
   recycle (warm start from the store). The recycle's figure is the floor.
5. **Leak rule (proposed, unverified):** the source is the same after every pair, so
   `allocated` after reclaim should plateau. Fit a least-squares line to `allocated` after
   reclaim against pair number (`soak.py`'s `slope`); call it a leak if the line adds more
   than 5% of the ready size over 1,000 pairs with r² above 0.8, and name the holder whose
   count or bytes grows with it. A run whose first sample has no `allocated` has no ready size
   to measure the line against: its verdict is unknown, never no leak.
6. Projects: ripgrep (fast, 1.6 GB), nushell (a mid-size reference with a recorded heap), and
   tracedecay once u1's build exists. Linux and macOS, each against its own baseline.

Settles: whether anything leaks; which holder; what item 1 returns and item 2 must cover.

### M2. #261 on Tempo (and reth for load)

With `RA_JIT_MEMORY`, `memsplit.py tempo` with its six one-line edits and then the two
`eprintln!` edits of the report. Per step: resident, jemalloc `allocated`, `RssAnon`, `VmHWM`,
and from the worker log whether the edit was a patch or a build in full (the `stats:` and
`edit-to-binary:` lines `memsplit.py` waits for, "compiling the program for edits to patch", and
the `no patch:` reason). Settles whether the 23.3 GB is a
build in full, an allocator high-water mark, or growth. Needs a host with more than 48 GB free;
this Mac has 48 GB and is shared (Tempo loads at 10 GB, reth at 19.6 GB).

### M3. Checkout ladder

One worker, `K` worktrees of `K` PR heads of one repository (K = 1, 2, 4, 8, 16), each loaded as
cargo would load it and then built once; record the worker's resident size and the ingredient
table after each. Then delete the worktrees in reverse and record again. Settles what a view
costs, and whether deletion returns anything (it should not today, item 3). A second ladder
with one worktree moved through `K` heads by file edits is the policy to compare.

### M4. Idle reclaim A/B

On the same worker after M1's last pair: solver caches alone, the type GC (which clears the
solver caches first on every thread and runs only when no thread solves: the collection is not
run without them, so the GC's own share is the difference from the caches alone), purge alone,
`background_thread:true` and one second of decay, each as its own run from the same state;
record the figure and the next edit's time (the page faults the purge costs), and how long
each collection of the types took (the `reclaim-types-us:` lines of `RA_JIT_STATS`). Settles
items 1 and 6's order.

### M5. Fleet shape: cores and allocator

M1 on a 96 or 128-core host unpinned, and pinned to 16 and 32 cores, with jemalloc's default
arenas and with `narenas` set to the pinned count; record `RssAnon`, `stats.retained` and
`VmHWM`. All published figures are pinned to 16 cores, so this is the unknown for the 128-core
box (**unverified**).

### M6. macOS footprint against live bytes

On a Mac, ripgrep and nushell-size projects: `ri_phys_footprint` (what `held_by` reads) beside
jemalloc `allocated` and `resident` after `arena.4096.purge`, at ready and after M1. The
macOS rows of benchmarks.md (nushell 11.5 to 12.5 GB, ripgrep 2.2 to 2.8 GB) are 1.4 to 2.2
times Linux's and predate jemalloc; this says how much is the allocator giving pages back
lazily, how much is the footprint counting compressed pages, and how much is the code. It is
what sizes a slot on the M1 Ultra and the 16 GB laptop (**unverified**).

### M7. Cold-build peak under a 16 GB budget

`coldprof.py --memory` on ripgrep, hyperfine and bat under `systemd-run -p MemoryMax=12G` (Linux)
and on the 16 GB laptop, with the standard library's unit fetched as a release does (`bench.py`'s
fix), before and after #203's direction. Settles item 4.

## Fork survey

All 18 forks of `indexable-inc/blaze` (`gh api graphql`, `repository.forks`), every branch
except `main` and the merge queue's `gh-readonly-queue/*` compared with upstream main at
`c8e5f6a88a`; commit headlines and bodies searched for memory, RSS, OOM, evict, LRU, salsa,
jemalloc, mimalloc, arena, intern, shrink, leak, footprint, cgroup, purge, heap, resident. The
search is of messages, not diffs. Forks of the older lineage (`rustd`, before the rename) show
their whole history as "ahead".

| Fork | Ahead of upstream main | Memory-related |
|---|---|---|
| ScriptedAlchemy/blaze (this fork) | `main` 0; `blaze-next` 55, `-push2` 92, `-step3` 104, `dist/u2-design` 1, `static-split-ends` 1 | `cd1caf0141` (LRU) and `a0b116443c`, `05d1bee206` (durability, taken out) on `blaze-next` (section 3); no other |
| tobi/blaze | `main` 0 (1 behind); `blaze-next` 55, `-push2` 90, `-step3` 96, `typeid-constructors` 2, `typeid-upstream-closures` 3, `static-split-ends` 1 | the same `blaze-next` commits; `typeid-*` is TypeId of closures, not memory |
| zale-lee/blaze | none (`main` only, level) | nothing |
| tomz-alt/blaze | none (136 behind) | nothing |
| RandazzoMaxime/blaze | `main` 5 (Windows live and PGO); `blaze-next*` 28 to 42 each, `windows-live` 4, `windows-cargo-mode` 1, `script-keys` 1, `pr-assets` 1, `static-split-ends` 1 | `cd1caf0141` and `a0b116443c` in `blaze-next-gate`; the `blaze-resolve-*`, `blaze-registry-*` and `blaze-j2-*` lanes are 0 ahead (merged) |
| gakonst/ra-jit | `blaze-next` 4, `-api` 9, `-cfg` 17, `-merge` 16, `-push` 8, `-txn` 6, `-uring` 18, `fix/cargo-install-git` 1 (#293), `fix/chain-project-workspace-wrapper` 1 (#294), `fix/eager-include-textual-macro` 1 (#292), `nanocode` 3, `static-split-ends` 1 | none in commits; the author filed #261 and #259 from Tempo and reth |
| dyc3/ra-jit | `static-split-ends` 1; the rest 0 | `describes-variables-gating` is #225 (merged, section 3) |
| annujk/ra-jit | none (403 behind) | nothing |
| alexhraber/ra-jit | none (422 behind) | nothing |
| hexuria/ra-jit | `main` 89 (the `rustd` lineage); `gol/splice-trait-shim` 4, `-pr` 1 (#237), `live-ios-sim-1023` 3 | none; "Decide drops at compile time" is codegen (matched on "drop"), "anchor(name, init)" is live state |
| danm-gh/indexable-inc-ra-jit | `static-split-ends` 1; the rest 0 | nothing |
| Tks2k4/ra-jit | none (782 behind) | nothing |
| NorthIsUp/ra-jit | the `rustd` lineage: `ci` 57, `live-latency-*` 84 to 141, `parity-async-join` 95, `store-gen` 37, `tiers` 139, `hydrate-knob` 1 (#24), `llvm-edit-regress` 1 | `store-gen` (`5add541e21`, generational copy-on-read for the disk store, closed as #4) and `661a95cc11` (store may take 32 GiB) are disk |
| sstraus/ra-jit | the same lineage (`ci`, `live-latency-*`, `store-gen`, `tiers`, `parity-async-join`) | disk only, as above |
| andrewgcodes/ra-jit | `devin/1792545168-async-build-scripts` 2 | none (build scripts on a thread) |
| degouville/rust-analyzer-jit | `main` 165 (the `rustd` lineage) | none |
| meta-introspector/ra-jit | the same lineage (`ci`, `live-latency-*`, `store-gen`, `bench-readme`) | disk only |
| wesleysimplicio/ra-jit | `main` 9 (README rewrites) | nothing |

One commit in the whole survey touches memory directly, `cd1caf0141` (the LRU), and a second
(`a0b116443c`, since taken out) is about what the LRU dropped; both are in the `blaze-next`
lineage under #283. No fork has a change to the allocator, the interner, the checkouts or the
ceiling.

## Not checked

- No diff of any fork commit but `cd1caf0141`; a memory change that its message does not name
  would be missed.
- Upstream's other issues and pull requests were found by keyword; closed pull requests older
  than the list's 400 newest were not read.
- `Tree::collect`'s callers on `blaze-next` were searched in `main.rs` and `tree.rs` only.
- The checked-in `SOAK.md` is from an older branch (`ra-jit-soak`); its memory ceiling has
  since moved (the rule that stops a worker none of whose checkouts exists was not found in
  `daemon.rs` at `c8e5f6a88a`, **unverified** that it was removed).
- No claim here covers Windows, or LLVM against Cranelift (every cited memory figure is Cranelift).

## Sources

Commands, all read-only: `git`, `rg`/`grep` and `sed` on this checkout; `gh issue view`,
`gh pr view`, `gh pr list`, `gh search issues|prs` on `indexable-inc/blaze`; `gh api graphql`
for the fork list, the branch comparisons and commit messages; `gh api repos/.../commits/SHA`
and `contents` for two fork commits and three `blaze-next` files. Diagnoses and measurements
here are those of andrewgazelka (the research pages, #36 to #57, #188, #197, #203, #242),
dyc3 (#225) and gakonst (#242, #259, #261, #293); the ranking is ours.
