---
status: current
summary: The hash-domain, cache-key grain, non-BLAKE3 hasher and environment-variable inventory of surviving source, including explicitly parked compiler consumers. The domains script checks entries against the tree.
---
# Caches: the inventory

This inventory follows the source after the V2 item-layer rewrite. It includes active libraries
and retained compiler source; [parked.md](../parked.md) names what is excluded from the build.
A listed key or record does not establish that its compiler consumer runs. Deleted daemon/query
producers and obsolete domains are absent; their history remains in Git.

A grain is the granularity of an entry’s key: `item`, `file`, `unit`, `crate`, `program`,
`checkout`, `process`, or `none` for inputs, fixtures and names that keep no result. Coarse
containers state why they exist and how their entries are keyed. Retained coarse semantic
caches are identified as integration work. The [domains script](../../../../scripts/src/bin/domains.rs)
requires every observed domain stem, non-BLAKE3 hasher and named environment variable to be
classified, and refuses stale classifications and duplicate domain definitions.

## The caches

| cache | grain | identity and retained value | owner and integration state |
|---|---|---|---|
| `mapped-images` | file | Mapped image spines, VFS text pages and shipped source-tree listings. These contain separately named files, not the deleted query database image. | blaze-image, blaze-vfs and blaze-cargo/toolchain; the compiler mounting path is parked. |
| `vfs-tree` | file | File contents, directory entries and journal records; names and bytes use distinct domains. | blaze-vfs; the driver checks filesystem observations. |
| `disk-stamps` | file | A file’s remembered content identity under its path and stat stamp, with the racy-stamp exclusion. | blaze-usercode/inputs; local observation reuse for traced runs, not a portable content identity. Compiler integration is parked. |
| `plan-queries` | file | Manifest, configuration and invocation reads, plus resolver/source identities; a plan combines these input records. | blaze-units, blaze-tree, blaze-source and blaze-resolve; compiler planning consumers are parked. |
| `request-certificates` | checkout | A request shape/tool base, its recorded reads and answer; a local slot projects its certificate and stamps. | blaze-certify; a certificate summarizes one completed request while its units/items keep their own keys. The old daemon writer is deleted. |
| `item-pages` | item | The remaining item-syntax identity; the deleted frozen dependency item-page store is not a live producer. | hir-def/item_tree/items; retained parked source. |
| `macro-defs` | item | Definition/call token hashes, hygiene, cfg, editions and syntax extensions; a file index locates per-item rows. | hir-expand/declarative/stored; retained compiler integration is parked. |
| `impl-tables` | crate | Retained frozen impl-table and candidate-set records. | hir-ty/method_resolution/frozen; the legacy container remains crate-grained and parked. Item-grain impl facts are replacement integration work. |
| `dep-bodies` | crate | Retained dependency-body records and portable IDs. | hir-ty/check_cache; the legacy dependency container is crate-grained and parked, distinct from the per-body item store. |
| `dep-check-outcome` | crate | A dependency check outcome under its check key and diagnostic settings. | blaze-diagnostics/cache; legacy crate-grained outcome, parked pending the item findings path. |
| `dep-consts` | crate | A legacy dependency constant-value key contains the crate check key and item identity. | hir-ty/consteval/deps; retained parked code, to be replaced by the item-store result when that integration is complete. |
| `item-store` | item | Body lowering, inference, MIR, borrow verdict and constant-value records; base identity plus the facts each step read. | hir-ty/body_store and blaze-item-hash; retained compiler code is parked. |
| `goal-store` | item | One canonical trait-solver goal and its solver/reading context. | hir-ty/next_solver/goal_store; retained compiler code is parked. |
| `records` | item | A value under its base, read-list and observed read values; the list is independently named. | blaze-record; this is not a whole-checkout answer cache. |
| `item-facts` | item | An item, type, scope, impl set or source-location fact; the query records the fact it actually read. | blaze-items, blaze-item-hash and hir-ty-mir; compiler-facing producers are parked. |
| `expansions` | item | One proc-macro expansion with its macro library, input tokens and observed environment/files. | proc-macro-api and blaze-macro-eval; retained integration is parked. |
| `macro-machine-code` | file | A proc-macro Wasm module and its engine-specific compiled bytes. | blaze-macro-eval/wasm; module content and engine bytes determine the key. Integration is parked. |
| `runs` | unit | One build-script or sandboxed-test invocation, provider, inputs and traced outside reads. | blaze-usercode; each invocation is a unit. Compiler integration is parked. |
| `warm-processes` | unit | A reusable user-code process under the inputs determining that process. | blaze-usercode/warm; retained parked source, separate from a persisted run answer. |
| `test-passes` | item | One test entry and its observed code/outside reads; suite and program-state tables index these entries. | blaze-program/reach and blaze-testsel; a suite is a container, not the key of each test result. Compiler integration is parked. |
| `unit-records` | unit | A unit signature and manifest identity, late reads and front-end settings; bodies stay separately keyed. | blaze-unitkey and blaze-command/front; the unit is the Cargo compilation context. Full command integration is parked. |
| `code` | item | A function instance, static, template or code-shaping fact, including reference-alias patterns. | blaze-codegen and hir-ty-codegen; retained parked source. The lexical inventory also includes retained test fixture domains. |
| `describes` | item | An instance/static plan or describe record under its target, toolchain, stage and read-addressed content. | hir-ty-codegen/v2/frontend/described; retained compiler integration is parked. |
| `kernel-entries` | item | A registered computation by its code, arguments and flags, checked against the facts it read. | blaze-kernel; the registry identifies definitions rather than replacing an entry’s key. |
| `devirt-world` | program | The retained devirtualization world, sites, object/type arguments and fixtures. | hir-ty-codegen; the current world is program-wide and parked. Its grain is inventoried debt, not the final design. |
| `fn-cache` | item | A pure closure code key under behavior identity, key format, target, nightly and overflow policy. | hir-ty-codegen/fncache; its exact pure key constructor is included by an active library test, while compiler execution stays parked. |
| `link-state` | program | Image state, section/head digests, native inputs and linker records; functions/statics retain individual identities. | blaze-link; whole-image state is the container needed to resume incremental placement. Its compiler consumer is parked. |
| `layout-profiles` | program | Profile images, weights, component/run identities and merged profile records. | blaze-profile and hir-ty-codegen; a profile describes a program/recording, not a semantic answer for every body. |
| `fetched` | file | Fetched objects, content chunks, their indexes and package placement. | blaze-fetch and blaze-pack; object/chunk content fixes its key. |
| `lean-proofs` | item | The retained Lean verdict identity. | hir-ty-lean; front-end integration is parked; standalone Lean proofs run separately. |
| `store` | item | Content-addressed values, schemas, pins, sealed item sets and their directory roots. | blaze-cas, blaze-format and blaze-gc; sets locate individually keyed items, not a coarser identity replacing them. |
| `remote` | file | Signed roots/key lists/bundles, shard indexes, segment values and global/pack manifests. | blaze-remote, blaze-global and blaze-pack; distribution containers are verified before their records are trusted. |
| `identity` | none | Code, configuration, source-root and other input identities consumed by the rows above; this row keeps no result. | Source-defined identities are inventoried without claiming all compiler consumers are active. |
| `not-keys` | none | Model/negative-control fixtures, oracle samples and domain-shaped non-cache text. | blaze-storemodel, kernel exhaustive controls, front-items cutoff tests and retained fixtures; no production answer is cached by this row. |
| `unaudited` | none | Remaining launch/rustc-stand-in identities whose producer audit is still outstanding. | Inventoried for uniqueness; this row claims no cache-validity proof. |
| `agent-sessions` | process | Agent hook session/token names, not compiler result keys. | blaze-cargo/cli/agents; a session is deliberately process-scoped. The compiler hook path is parked. |
| `plain-store` | file | Physical segment/log/root identities, namespace names, boot and nonce checks. | blaze-cas/plain; segments pack individually keyed records. Boot/nonce names do not stand for compiler answers. |
| `typed-records` | item | Declared kind/code identities and read-addressed values under a key, read shape and observed answers. | blaze-keyed; kinds determine key fields and record roots, and a checked record carries its full key. |
| `program-items` | item | Function type arguments/settings and static-value records for the program walk. | blaze-program; stable symbols are distinct from content-dependent plan/code/value keys. |
| `program-manifests` | program | An ordered program manifest, its content chunks and kept image rows. | blaze-program and blaze-command/program; the manifest describes the whole program, while rows name individual plan/code/static keys. Full compiler wiring is parked. |
| `solo-sessions` | checkout | The no-daemon prototype command’s checkout namespace, pin and answer. | blaze-solo; a checkout pin names the observed world/session, while computations keep their kernel identities. |
| `surface-names` | item | A module export set, independent of exported bodies. | blaze-surface/module_scope; the module is the item whose public names are described. |
| `file-items` | item | An outlined item, signature, head and sibling context derived from parser events. | blaze-file-items; this layer retains neither a syntax tree nor a database. |
| `front-syntax` | item | Per-item IDs, headers, bodies, names and impl gates, plus their file/reading context. | blaze-front-items; file/crate tables contain independently digested parts. |
| `front-settings` | process | The front-end settings singleton and individual setting fields used by its session. | base-db/settings; settings apply to a database/session rather than one item. This retained integration is parked. |
| `own-effects` | item | A body’s own effects and their shape before transitive callee effects are folded in. | hir-ty-pure/own; this per-body fact feeds the transitive summary. Compiler integration is parked. |
| `kernel-sessions` | process | Verified rows and read postings, rooted by a pin and stored in immutable keyed trie nodes. | blaze-kernel/session and trie; a session contains independently validated computation rows, not their semantic key. |
| `kernel-tables` | item | A computation value partitioned into keyed parts, key-set digests and a deterministic Merkle index. | blaze-kernel/table; a part reader depends on its part, not every part of the owner. |
| `launch-records` | item | A function/static launch/bind record, named places and text/string data. | blaze-launch-host; each callee/static has its own bind key. Compiler integration is parked. |
| `launch-images` | program | A kept launched-program start state and the next launcher’s answer. | blaze-launch-host/start; this restart container references individually keyed records. Its old daemon consumer is deleted. |
| `command-graphs` | checkout | A request’s unit graph, checkout/program namespace, target/features and resolved source/tool settings. | blaze-command; request-wide setup does not replace unit/body keys. Compiler dependencies keep this crate parked. |
| `outside-reads` | none | Current file, directory, environment and existence observations. | blaze-host/outside and blaze-solo/world; inputs to record validation, not another cache of completed work. |

## Switches that select a caching path

The scanner counts a literal name even for a build-time input, a scrubbed legacy setting,
a route-validation name or a test-module constant. The inventory does not imply that each
variable is a live runtime control. Names removed with the old daemon are not retained.

| variable | what it selects | owner and lifetime |
|---|---|---|
| `BLAZE_PROC_MACRO_CACHE=check` | Re-expands a kept expansion and compares its stored result; normal mode is Keep, and every other value is refused. | The retained proc-macro-api checker; verification mode stays when the parked compiler integration returns. |
| `BLAZE_CACHE_TRANSPORT` | Explicit HTTP/1.1 or HTTP/3 for anonymous public-cache reads. Signed requests use HTTP/1.1. Record keys/representations are unchanged. | blaze-remote; both protocols are intentional. Failure of the chosen protocol is an error, not implicit fallback. |

### Remote settings

`BLAZE_CACHE_ENDPOINT`, `BLAZE_CACHE_BUCKET` and `BLAZE_CACHE_PUBLIC_URL` identify the service.
`BLAZE_CACHE_PUBLIC_ANCHORS` fixes public trust. `BLAZE_CACHE_ACCESS_KEY_ID`,
`BLAZE_CACHE_SECRET_ACCESS_KEY`, `BLAZE_CACHE_SIGNING_SEED` and `BLAZE_CACHE_KEY_LIST` supply
writer credentials and signing authority. `BLAZE_CACHE_ANCHOR_SEED` is an explicit key-list
creation input. The settings reader refuses values simultaneously supplied in a file and
the environment. These configure access to the same signed formats, not parallel validity rules.

### Observation, replay and process controls

`BLAZE_DIST_EVENTS` appends actual distributed transitions as locked JSON lines. This is
observation IO, not a cache representation or key selector. `BLAZE_DIST_TEST_RESULT_GATE`
captures a real lead’s result and suspends its sender’s heartbeats until release or its finite
deadline, allowing acceptance to demonstrate fenced stale-result rejection. Both belong to the
retained carrier and do no file IO when unset.

`BLAZE_TEST_LIBRARY_DOWNLOAD` and `BLAZE_TEST_LIBRARY_FETCH` isolate download/fetch child tests.
`BLAZE_SIM_SEED`, `BLAZE_SIM_SEEDS` and `BLAZE_SIM_TAPE` select simulation replay or coverage.
`BLAZE_DAEMON_EXIT_UNHELD` remains named by the parked thin-client route-consistency check;
the daemon implementation it referred to was deleted.

`BLAZE_AUDIT_DB` selects the advisory database whose tree the retained audit command hashes.
`BLAZE_BSTD_DIGEST` is the build-time identity of the shipped bstd archive, not a runtime cache
mode. Other source-named settings cover diagnostics, toolchain/code identities, resource limits,
test probes, front-end/backend policy and explicit environment scrubbing:

`BLAZE_ALLOC_TRACE`, `BLAZE_ASSEMBLER`, `BLAZE_BACKEND`, `BLAZE_BENCH_MACHINE`, `BLAZE_BIN`, `BLAZE_BORROWCK`,
`BLAZE_BORROWCK_DEBUG`, `BLAZE_BUILT_IN`, `BLAZE_CHECK_PLACES`, `BLAZE_CODEGEN_CODE`, `BLAZE_CORES`, `BLAZE_DUMP_CLIF`,
`BLAZE_DUMP_LLVM`, `BLAZE_DUMP_MIR`, `BLAZE_DUMP_TIER`, `BLAZE_EMIT`, `BLAZE_FN_CACHE_TRACE`, `BLAZE_FRONT_END_CODE`,
`BLAZE_FUZZ_RUNS`, `BLAZE_GC_TEST_HOLD_LEASE`, `BLAZE_GIT_DATE`, `BLAZE_GIT_SHA`, `BLAZE_HASH_ORDER_CHILD`, `BLAZE_HISTORY_LEVEL`,
`BLAZE_HOME`, `BLAZE_HUGE_PAGES`, `BLAZE_IGNORE_PIN_FOR`, `BLAZE_INLINE`, `BLAZE_INLINE_ASM`, `BLAZE_INPROC`,
`BLAZE_LAYOUTS`, `BLAZE_LEAN`, `BLAZE_LEAN_PROVER`, `BLAZE_LIBRARY`, `BLAZE_LIBRARY_FROM`, `BLAZE_LLVM_INLINE_THRESHOLD`,
`BLAZE_LOCK`, `BLAZE_MEMORY_BUDGET`, `BLAZE_MIR_OPT`, `BLAZE_NO_HOME`, `BLAZE_OPT`, `BLAZE_ORACLE_MINUTES`,
`BLAZE_ORACLE_OUT`, `BLAZE_PINNED_BY`, `BLAZE_PROFILE_CAP_MB`, `BLAZE_PROFILE_DETAIL`, `BLAZE_PROMOTE`, `BLAZE_RECORD_CODE`,
`BLAZE_REGALLOC`, `BLAZE_RELEASE`, `BLAZE_RELEASES`, `BLAZE_REVISIONS_SEED`, `BLAZE_REVISIONS_STORE_CHECK`, `BLAZE_RUN_AS_DEFAULT`,
`BLAZE_RUST_DIST`, `BLAZE_SANDBOX`, `BLAZE_SANDBOX_TEST_ALONE`, `BLAZE_SCRIPT_RUSTC`, `BLAZE_STATS`, `BLAZE_TOOLCHAINS`,
`BLAZE_TRACE_HOLDERS`, `BLAZE_UI`, `BLAZE_UI_TRANSCRIPT`, `BLAZE_VERIFY_STORE`, `BLAZE_VERSION`, `BLAZE_VERSION_LINE`,
`BLAZE_WARM_SOCKET`.

## Hash functions

Content identities use BLAKE3 under a domain, retaining at least 128 bits. Other classes are
`probe` (the key is compared after selecting a slot), `required` (another format’s algorithm),
`integrity`, `random`, `names` (recognizing a hasher rather than using it), and `unaudited`.

| where | function | class | what equality means |
|---|---|---|---|
| `crates/blaze-ids/` | BLAKE3, 256 bits or the first 128 | identity | Content identity under a domain. |
| `crates/blaze-hash/` | foldhash, SipHash and prehashed key words | probe | The map compares complete keys after probing. |
| `crates/blaze-memo/src/lib.rs` | FNV-1a, 64 bits | probe | Kind names choose table hashes; kinds compare by their static Kind addresses, so a collision is not identity equality. |
| `crates/blaze-std/src/lib.rs` | Re-exported hashers | names | Public names, not another implementation. |
| `crates/hir-ty/src/method_resolution/frozen/pages.rs` | FNV-1a, 64 bits | probe | The persisted slot is followed by decoding and comparing the entry key. |
| `crates/hir-ty-codegen/type_id.rs` | StableSipHasher128 | required | Rust TypeId compatibility. |
| `crates/blaze-units/src/load/plan/hash.rs` | StableSipHasher128 | required | Cargo metadata identity. |
| `crates/blaze-source/src/source_id.rs` | StableSipHasher128 | required | Cargo-compatible source and registry directory identity. |
| `crates/blaze-profile/src/flight/postmortem.rs` | FNV-1a, 64 bits | identity | The recorder’s existing layout word; a collision can confuse layouts. Inventoried debt, not a BLAKE3 content-identity claim. |
| `crates/blaze-profile/src/panics.rs` | FNV-1a, 64 bits | probe | Panic-site counters; colliding sites can combine counts. |
| `crates/blaze-std/src/rt.rs` | RandomState | random | A runtime seed, not persisted content identity. |
| `crates/ix-prof/src/lib.rs` | RandomState | random | A recording-session nonce. |
| `crates/hir-ty-codegen/fncache.rs` | Recognized RandomState names | names | The function-cache purity model. |
| `crates/hir-ty-pure/` | Recognized RandomState names | names | The purity/effect model. |
| `crates/harness/` | DefaultHasher and FNV-1a | unaudited | The separately built harness application; producer audit remains open. |

Other format-required hashes are not scanned by this rule: SHA-256 for registry/lock/pin
checksums, MD5 or SHA-1 where debug formats require them, and CRC32 for GNU debug links.
The scanner’s coverage does not establish that these are every algorithm in the repository.

## Domains

Each name is the source domain less its trailing version number. Without `=`, it has the prefix
`blaze v2 `; a leading `=` spells the whole stem. Versioned variants share one entry. An absent
producer is removed, and a new producer needs a row explaining its key grain.

- `mapped-images`: `image spine`, `tree texts`, `vfs disks 2: disk.rs's encoding`, `vfs file text 1: a file's bytes under its FILE_DOMAIN content id`, `vfs image`
- `vfs-tree`: `vfs directory`, `vfs file`, `vfs journal record`, `vfs name index key`
- `disk-stamps`: `disk stamp`, `disk view id stamp`
- `plan-queries`: `cargo asked`, `cargo asked key`, `cargo asked or not`, `cargo config`, `cargo config key`, `cargo config read`, `cargo configured`, `cargo dir`, `cargo dir key`, `cargo file`, `cargo file key`, `cargo fixed dir`, `cargo fixed dir key`, `cargo fixed file`, `cargo fixed file key`, `cargo foreign`, `cargo foreign key`, `cargo found dir`, `cargo found file`, `cargo invocation`, `cargo invocation key`, `cargo manifest`, `cargo manifest key`, `cargo manifest parsed`, `cargo mount index`, `cargo mount table`, `cargo mounts`, `cargo mounts key`, `cargo plan`, `cargo plan key`, `cargo planned`, `cargo std key`, `cargo std placement`, `cargo std planned`, `cargo std units`, `cargo tree`, `cargo tree key`, `cargo tree printed`, `cargo units`, `cargo workspace`, `cargo workspace key`, `cargo workspace read`, `=blaze workspace build dir`, `=blaze-resolve package id`, `=blaze-source interned string`, `=blaze-source registry name`, `=blaze-source source id`
- `request-certificates`: `request slot`, `request slot body`, `request slot names`, `serve request certificate`, `serve request certificate variable`
- `item-pages`: `item syntax`
- `macro-defs`: `expansion key`, `file entry set`, `file index`, `item attrs`, `item body`, `item header`, `item tokens`, `macro def squeeze`, `macro def tokens`, `whole item hash`
- `impl-tables`: `=blaze frozen impls`, `impl tables`
- `dep-bodies`: `dependency bodies`, `dependency body`, `dependency runtime MIR`, `hir-def body tree`, `ported ids and types`
- `dep-check-outcome`: `dependency check outcome`
- `dep-consts`: `dependency constant value`
- `item-store`: `body entry`, `item early identity`, `item identity crate inputs`, `item identity front end input`, `item presentation`, `item store`, `item store 1 answer record`, `item store 1 body`, `item store MIR`, `item store borrow check verdict`, `item store constant value`, `item store inference`, `item store lowering`, `item store served answer`, `item store step value 1 MIR`, `item store step value 1 inference`, `item-hash absent`, `item-hash attrs`, `item-hash crate names`, `item-hash crate root`, `item-hash fields`, `item-hash match check`, `item-hash traits in scope`, `item-hash whole scope`
- `goal-store`: `goal`, `goal crate`
- `records`: `record answer`, `record list`
- `item-facts`: `=blaze fact relied`, `bound derive method`, `bound hash`, `builtin derive signature`, `closure code hash`, `code hash`, `coroutine code hash`, `deep hash`, `fact`, `fact auto traits`, `fact block impls`, `fact captures`, `fact code`, `fact coherence`, `fact const value`, `fact const value type`, `fact crate`, `fact derive`, `fact drop check`, `fact field`, `fact hidden types`, `fact implements`, `fact inherent impls`, `fact layout`, `fact method`, `fact name`, `fact resolves`, `fact rpitit types`, `fact scope traits named`, `fact signature`, `fact specializes`, `fact target`, `fact trait impls`, `fact traits named`, `fact vtable`, `field order`, `field order plan`, `form`, `form spans`, `index impls`, `index scope`, `interface hash`, `item key`, `item-hash crate settings`, `item-hash escaped`, `item-hash escaped item`, `item-hash field order`, `item-hash inherent assoc types`, `item-hash modules`, `item-hash setting`, `keyed ref`, `layout`, `layout hash`, `mir source names`, `name`, `seen impls`, `signature hash`, `simplified type`
- `expansions`: `expansion reads gone`, `macro-eval code`, `passed environment`, `proc-macro checked expansion`, `proc-macro library`, `verified expansion values`
- `macro-machine-code`: `wasm engine bytes`, `wasm proc macro machine code`, `wasm proc macro module`
- `runs`: `copy origin`, `disk entry look`, `provider scope`, `provider seed`, `pure script identity`, `pure script instance`, `root path`, `run depends on its roots`, `run file shown`, `run given`, `run given part`, `run shape slot`, `run traced`, `shape id`, `test attributes`, `traced listing`, `unverified copies`
- `warm-processes`: `warm key`
- `test-passes`: `=blaze program test`, `testsel program state`, `testsel unlogged`, `=blaze-testsel ask`, `=blaze-testsel env value`, `=blaze-testsel test attrs`, `=blaze-testsel test env`, `=blaze-testsel test item`, `=blaze-testsel test suite`
- `unit-records`: `diagnostics emitted`, `=blaze-command deferred late read`, `=blaze-command unit settings`, `=blaze-unitkey manifest identity and printed names`
- `code`: `=blaze asm`, `=blaze global asm`, `aliased shape`, `allocator`, `allocator item`, `asm`, `asm item`, `behaviour identity`, `behaviour identity leaf`, `body code`, `built function entry`, `built id`, `built in`, `built static entry`, `built static key`, `cached function`, `code`, `code id`, `code identity inlined`, `code identity name`, `code identity value edges (test)`, `code with names read`, `codegen binding names`, `codegen discriminant`, `codegen names read`, `codegen owned code`, `codegen package name`, `codegen policy`, `codegen settings`, `codegen span table`, `codegen template code`, `codegen told types`, `compile identity`, `compile identity callee`, `compile identity value`, `compile identity walk`, `compiled code print`, `counter`, `data`, `debug name`, `edits fixture code`, `exec bound`, `exec fn id`, `exec placed`, `exec span table`, `exec static`, `exec static value`, `fold data`, `fold ir`, `fold references`, `fold shape`, `folded`, `folded identity`, `function digest`, `global_asm object`, `group flight`, `identity adt`, `identity args`, `identity bound`, `identity const`, `identity type`, `imports`, `instance`, `instance abi`, `instance bound`, `instance identity`, `link span table`, `live assumed unhashable`, `masked`, `multiversion plan`, `own place shape`, `owned data`, `placed`, `placed args`, `recursion shim body`, `reference aliases`, `reference placed`, `reference placement`, `relative flight`, `relative shape`, `release imported bodies`, `release laid out`, `renamed owned data`, `shape`, `share fact`, `shared args`, `shim`, `side`, `start`, `static digest`, `static value`, `store folded`, `store masked`, `store release template`, `store side`, `store static`, `store summary`, `store thunk`, `subject`, `summarized`, `target`, `template`, `template own span table`, `test listing`, `told args`, `toolchain id`, `why dev template step`
- `describes`: `codegen static`, `describe record`, `describe record stage`, `instance plan name`, `launch naming`, `static plan name`
- `kernel-entries`: `kernel identity`, `kernel registry`
- `devirt-world`: `devirt code`, `devirt fixture`, `devirt object`, `devirt object args`, `devirt site`, `devirt world`
- `fn-cache`: `cached code`
- `link-state`: `link asan global name`, `link asan globals`, `link asan module name`, `link asan odr indicator`, `link cie`, `link debug record`, `link file`, `link file block`, `link file name`, `link file node`, `link image`, `link keyed leaf`, `link keyed node`, `link keyed set`, `link lineage`, `link msan option`, `link sancov bound`, `link sanitizer constructor`, `link sanitizer constructor pointer`, `link target autolinked libraries aarch64`, `link target autolinked libraries x86_64`, `link target default libraries`, `link target re-exported libraries aarch64`, `link target re-exported libraries x86_64`, `link target script`, `span table`, `=blaze-link kept state`, `=blaze-link kept state head`, `=blaze-link kept state output`, `=blaze-link kept state section`
- `layout-profiles`: `args`, `link heat`, `link layout`, `perf component`, `perf deep`, `perf image`, `perf merged`, `perf rep`, `perf run`, `pgo name`, `pgo profile`
- `fetched`: `=blaze pack chunk`, `=blaze pack object`, `=blaze-fetch`, `=blaze-fetch chunk: a piece of an object by its BLAKE3`, `=blaze-fetch index record: form, BLAKE3, length`, `=blaze-fetch object as an input`, `=blaze-fetch object: bytes, or chunks of 4194304 bytes, by their BLAKE3`, `=blaze-fetch origin`
- `lean-proofs`: `lean step`
- `store`: `=blaze gc lease nonce`, `buffer`, `hashcons pack`, `schema`, `store chunk`, `store cold unit`, `store dictionary`, `store key`, `store pin`, `store sealed set`, `store set leaf`, `store set root`, `store staging`, `store value`, `zstd dictionary`
- `remote`: `global bundle`, `global manifest`, `global manifest key`, `global piece`, `global record key`, `=blaze-remote bundle`, `=blaze-remote key list`, `=blaze-remote root`, `=blaze-remote shard block`, `=blaze-remote shard head`, `=blaze-remote toolchain`, `=blaze-remote value`
- `identity`: `=blaze error union member`, `=blaze shipped code identity`, `codegen code`, `codegen code input`, `crate graph`, `crate ported files`, `crate workspace data`, `front end code`, `source root content without other crates' OUT_DIRs`, `target data`, `unit record code`
- `not-keys`: `=blaze fetch-url's request is not UTF`, `=blaze storemodel code`, `=blaze storemodel crash check`, `=blaze storemodel disk`, `=blaze storemodel expectation`, `=blaze storemodel fact`, `=blaze storemodel input`, `=blaze storemodel key`, `=blaze storemodel open crash check`, `=blaze storemodel pin`, `front cutoffs test`, `kernel exhaustive fact`, `kernel exhaustive input`, `kernel exhaustive pin`, `kernel planted identity only`, `verify store sample`, `=blaze-bench-protocol`
- `unaudited`: `=blaze launch file name`, `=blaze launch location`, `=blaze pin lookup`, `launch slot`, `rustc stand-in library directory`, `rustc stand-in library root`, `rustc stand-in probe view`
- `agent-sessions`: `=blaze agents session`, `=blaze agents token`
- `plain-store`: `=blaze plain store boot`, `=blaze plain store log`, `=blaze plain store namespace`, `=blaze plain store nonce`, `=blaze plain store root`, `=blaze plain store segment`
- `typed-records`: `=blaze-keyed address`, `=blaze-keyed code key`, `=blaze-keyed shape slot`
- `program-items`: `=blaze program args`, `=blaze program settings`, `=blaze program static`
- `program-manifests`: `=blaze program chunk`, `=blaze program manifest`, `=blaze-command program image rows`
- `solo-sessions`: `=blaze solo answer`, `=blaze solo checkout`, `=blaze solo pin`
- `surface-names`: `=blaze surface exports`
- `file-items`: `file item`, `file item head`, `file item siblings`, `file item signature`
- `front-syntax`: `front carry step code`, `front crate`, `front file`, `front item base`, `front item body record`, `front item header`, `front item id`, `front item impl gates`, `front item impl scope`, `front item names`, `front reading`
- `front-settings`: `front setting field`, `front settings`
- `own-effects`: `hir-ty-pure own effects`, `hir-ty-pure own effects shape`
- `kernel-sessions`: `kernel session name`, `kernel session node`, `kernel session root`
- `kernel-tables`: `kernel table branch`, `kernel table branch keys`, `kernel table key`, `kernel table leaf`, `kernel table leaf keys`, `kernel table part`
- `launch-records`: `=blaze launch str`, `=blaze launch text`, `launch bind`, `launch symbols`
- `launch-images`: `launch start`, `launch start body`
- `command-graphs`: `=blaze-command checkout`, `=blaze-command code settings`, `=blaze-command dir`, `=blaze-command graph`, `=blaze-command library`, `=blaze-command program`, `=blaze-command settings`, `=blaze-command source`, `=blaze-command target`, `=blaze-command tool`, `=blaze-command view`, `=blaze-command walk features`, `=blaze-command walk target`
- `outside-reads`: `front carry exists`, `=blaze-host outside directory`, `=blaze-host outside gone`, `=blaze-host outside unset`, `=blaze-host outside variable`
