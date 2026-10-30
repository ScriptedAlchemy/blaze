# Bazel with NativeLink, for Rust, against blaze's distributed design

This survey, dated 2026-10-25, is unit u3 of the distributed program. It examines
Rust compilation and caching in Bazel, rules_rust and NativeLink, then proposes
changes to blaze's [distributed design](../distributed.md). It reviews PR
[#1](https://github.com/ScriptedAlchemy/blaze/pull/1) at `45449ae6` on `dist/u2-design`.

A claim marked **read** comes from a source at a pin below. **reported** means
another document or person reports it without a recheck here. **unverified** marks
an inference. This survey read source text without building or running code.

## Sources

| Name | Repository | Pin | Read |
|---|---|---|---|
| REAPI | bazelbuild/remote-apis | `c2c07e3d718d3d490a4958344d9c3e47de0bfaf3` (main, 2026-10-20) | `build/bazel/remote/execution/v2/remote_execution.proto` (below `re.proto`), `platform.md` |
| Bazel | bazelbuild/bazel | tag `9.3.0`, `e89755babf3e941bc1e9d8120b1cd48854ab9dfa` | `src/main/java/com/google/devtools/build/lib/` (below `lib/`): `remote/`, `dynamic/`, `exec/ExecutionOptions.java`, `actions/`; `site/en/remote/*.md`; three files of `lib/analysis/test/` fetched one by one from `raw.githubusercontent.com` at the tag |
| rules_rust | bazelbuild/rules_rust | tag `0.74.0`, commit `a2df978962d26028897b72380ba2075cc5272b10` | `rust/private/`, `rust/settings/`, `cargo/private/`, `util/process_wrapper/` |
| NativeLink | TraceMachina/nativelink | `7e55aeb5df35413c1e2064b104fec2df93583ae9` (main, 2026-10-26) | `nativelink-config/src/`, `nativelink-scheduler/src/`, `nativelink-worker/src/`, `nativelink-service/src/`, `nativelink-store/src/` |
| sccache | mozilla/sccache | `cf5efe611778e6ec473c9d051f5215d42833d446` (2026-10-25) | `docs/`, `src/compiler/rust.rs` |
| buck2 | facebook/buck2 | `5f9904ca597e205f7f9b92e55a9f31311977d0e1` (2026-10-25) | `prelude/rust/`, `docs/users/` |
| tracedecay | ScriptedAlchemy/tracedecay | `d79de8227384eea80a21304aa835fff7568a8006` | git objects only (the owner's working tree has uncommitted edits); plus the untracked note `.codex/pstack-runs/pr-merge-sweep/ci-scheduling-measurement-analysis.md` (below "the note") and its `ci-scheduling-measured/critical-rust-actions.json` |
| blaze | this fork | `dist/main` `c8e5f6a8`; design `dist/u2-design` `45449ae6` | `crates/ra-jit/src/remote.rs`, `remote/`, `crates/ra-jit-store/src/lib.rs`, `docs/distributed.md` |
| reviews | `orchestrate/blaze-distributed/reviews/` | u2-review-a, u2-review-b as of 2026-10-25 22:01 | both read in full |

A citation `path:N` resolves as `https://github.com/OWNER/REPO/blob/PIN/path#LN` with the pin
above. tracedecay pins rules_rust 0.74.0 (`MODULE.bazel:7`) and Bazel 9.3.0 (`.bazelversion`),
which is why those tags were read; the measured run used Bazel 9.3.1 (the note, line 9). The
fleet's NativeLink version is not in tracedecay's repository, so which NativeLink features below
the fleet has is **unverified**. Issue texts were read with `gh issue view` (rust-lang/cargo#5931,
indexable-inc/blaze#226), nothing written.

## Short version

- **REAPI keys a whole action.** The action digest covers argv, the declared environment, output
  paths, the working directory, every input file by content in one Merkle tree, the platform
  properties, the timeout and a salt (**read**, `re.proto:674-739`). The worker's undeclared
  environment is not in it (`re.proto:746-748`). There is no batch ActionCache read
  (`re.proto:170`).
- **Bazel's unit is the action; rules_rust's action is one rustc of one crate.** A crate is one
  `Rustc` action, plus a `RustcMetadata` one with pipelining (off by default) and a
  `CargoBuildScriptRun` per build script (**read**). No incremental, no persistent worker. So
  the `tracedecay` crate is one 200.846 s action on one worker, holding a `cpu_count=1`
  reservation (**read**, the note line 25, `scripts/bazel/platforms/BUILD.bazel:12`).
- **Every tracedecay Rust action declares 5.2 to 5.66 GB and about 33,000 files** (**read**,
  the JSON). The client paid 0.26 to 0.44 s an action to upload what was missing; the worker paid
  1.6 to 2.0 s to set up four of the five and 39.7 s for one (**read**). rules_rust puts the whole
  Rust sysroot and the whole C++ toolchain into every Rustc action (**read**); that this is most of
  the 5.2 GB is **unverified**.
- **NativeLink trusts by who can reach it, not by what it signs.** Auth is mTLS or the network; an
  ActionResult carries no proof that it came from running its Action (**read**). tracedecay
  reaches it in plaintext gRPC over the tailnet, with one instance `main` for `master` and
  same-repository pull requests, and Bazel's default lets a client upload results of actions it
  ran locally (**read**). Whether the fleet's AC refuses client writes is **unverified**.
- **NativeLink's workers outlive actions and share mutable state**: a hardlinked input cache, and
  persistent worker processes. Its own docs say read-only inputs "are not a security boundary"
  and offer `single_use` workers for isolation (**read**). The reviewers' trust blocker on blaze's
  warm slots is a problem NativeLink has too; blaze should not copy it.
- **Bazel caches test passes by action key** and reruns only on changed inputs, an `external`
  tag, `--runs_per_test` or a previous failure (**read**). That is the precedent for blaze's
  `TestKey`. It has the blind spots review-b names (time, randomness; **unverified** for Bazel's
  key), and a declared runfiles tree that blaze's design lacks.
- **NativeLink separates content identity from execution identity**: identical cacheable actions
  share one execution, each client keeps its own operation id, and updates are versioned
  (**read**). That is the fix review-a M6 asks for.
- **Where tracedecay's time went** in the one measured run: 51.8% of the critical uncached Rust
  time queued, cause not identified; 487.687 s in a crate_universe git submodule fetch; transfers
  material only for one action (**read**, the note). The run was a `k8-opt` release build, which
  blaze's design leaves on Bazel.
- **Verdict on REAPI during migration:** do not build a REAPI client in this program. Keep it as
  #226's optional edge after per-crate records exist. Reasons in section 7.9.

## 1. The REAPI v2 model

### Services

- **Execution.** The client uploads the inputs, the `Command` and the `Action` to the CAS, then
  calls `Execute` with the action digest and reads a stream of `Operation`s (**read**,
  `re.proto:44-71`, `118`). The server need not run an action at most once: "The server MAY
  execute the action multiple times, potentially in parallel" (`re.proto:113-117`).
  `WaitExecution` reattaches by operation name (`re.proto:136`). Errors in setup, such as a
  missing input, come back as `FAILED_PRECONDITION` with the missing digests (`re.proto:86-108`).
- **ActionCache.** `GetActionResult` and `UpdateActionResult`, both keyed by the action digest
  (**read**, `re.proto:157-196`). A server SHOULD keep the CAS blobs a returned result names
  available for a while (`re.proto:158-165`). An update requires the Action and Command in the
  CAS first, "to allow the server to perform access control based on the type of action"
  (`re.proto:175-181`).
- **ContentAddressableStorage.** `FindMissingBlobs`, `BatchUpdateBlobs`, `BatchReadBlobs`,
  `GetTree`, and the newer chunking calls `SplitBlob`, `SpliceBlob` (**read**,
  `re.proto:350-624`). Large blobs go over ByteStream as
  `{instance}/uploads/{uuid}/blobs/{hash}/{size}` and compressed as
  `.../compressed-blobs/{compressor}/{hash}/{size}`, the digest always of the uncompressed bytes,
  checked by the server (`re.proto:219-260`). Empty blobs always exist (`re.proto:342`).
- **Capabilities.** `GetCapabilities` returns the digest functions, compressors, batch limit,
  whether the AC takes updates and whether execution is on (**read**, `re.proto:635-646`,
  `2206-2470`).

### Action, Command, input root

- **Action** (`re.proto:674-739`, **read**): `command_digest` (678), `input_root_digest` (687),
  `timeout` (715), `do_not_cache` (719), `salt` (729), `platform` (739).
- **Command** (`re.proto:749-900`, **read**): `arguments` (775), `environment_variables`, sorted
  by name (783-785), `output_paths` (871), `working_directory` (886).
- **Input root.** A `Directory` lists files, directories and symlinks, each sorted, a file by
  digest and executable bit; each `Directory` is a blob of its own, so the input root is a Merkle
  tree (**read**, `re.proto:981-999`, `1048-1060`, `1094-1108`, CAS comment `205-213`).
- **Platform.** Name and value pairs, sorted (**read**, `re.proto:934-969`). "The server MAY
  choose to execute the action on any worker satisfying the requirements, so the client SHOULD
  ensure that running the action on any such worker will have the same result"
  (`re.proto:731-734`). The lexicon names `OSFamily` and `ISA` (`platform.md:11`, `26`).
- **What is not declared is the implementation's.** "the environment (such as which system
  libraries or binaries are available, and what filesystems are mounted where) is defined by and
  specific to the implementation" (**read**, `re.proto:746-748`).
- **ActionResult** (`re.proto:1250-1416`, **read**): output files, directories and symlinks by
  digest, `exit_code` (1387), stdout and stderr digests (1400), and `ExecutedActionMetadata`
  (1416): the worker, and timestamps for queued, worker start, input fetch start and end,
  execution start and end, worker completed (`re.proto:1187-1229`).

### What goes into the action key and what does not

| In the action digest (**read**) | Not in it (**read**) |
|---|---|
| argv; the declared environment, sorted | the instance name (`ExecuteRequest.instance_name`, `re.proto:1584-1590`): a separate namespace on the server, chosen by the server |
| output paths, working directory | `RequestMetadata` (tool, invocation, action mnemonic, target, configuration), sent as the `requestmetadata-bin` header (`re.proto:2610-2651`) |
| every input file's path, digest and executable bit, through the root digest | priority (`ExecutionPolicy`, `ResultsCachePolicy`, `re.proto:1555-1579`) |
| platform properties | `skip_cache_lookup` (`re.proto:1592-1605`), `digest_function` (`1629`), inline-output flags |
| the timeout: "two `Actions` with different timeouts are different", so a too-short timeout is not hidden by a hit (`re.proto:691-714`) | whatever the worker provides without declaring it (`re.proto:746-748`) |
| `do_not_cache`, `salt` | |

Bazel's salt holds whether the spawn may run remotely, an optional workspace name and a scrubbing
salt (**read**, `lib/remote/RemoteExecutionService.java:377-394`). `salt` exists "to place this
`Action` into a separate cache namespace ... and allows disowning an entire set of ActionResults
that might have been poisoned" (`re.proto:721-728`).

One consequence for tracedecay (**unverified** inference from the two sources): its exec
property `cpu_count` is a platform property, so the scheduling hint is part of every Rust action's
digest, and changing it from `1` would miss every cached Rust action.

### Lookups, batches, compression, digest functions

- **FindMissingBlobs** takes a list of digests (**read**, `re.proto:1846-1865`). Bazel splits the
  list to fit the gRPC message limit (`lib/remote/GrpcCacheClient.java:166-182`, `314-336`) and
  deduplicates concurrent queries, keeping "present" answers until invalidated
  (`lib/remote/RemoteExecutionCache.java:92-98`, `380-396`).
- **Batch limits.** The server advertises `max_batch_total_size_bytes` (`re.proto:2431`);
  NativeLink advertises 64 KiB (**read**, `nativelink-service/src/capabilities_server.rs:40`,
  `175`). Above it, ByteStream.
- **No batch AC read.** `GetActionResult` takes one action digest (**read**, `re.proto:170`,
  `1775-1811`). A client that wants 500 results makes 500 calls.
- **Compression.** zstd, deflate, brotli by resource name (**read**, `re.proto:2393-2410`, `2442`).
  NativeLink offers zstd only when an instance enables it (`capabilities_server.rs:155-162`). Bazel
  leaves it off by default (`lib/remote/options/RemoteOptions.java:497`).
- **Digest functions.** SHA-256 through `SHA256TREE` (`re.proto:2308`), `BLAKE3` (`2312`),
  `GITSHA1` (`2319`), named per request (**read**). NativeLink advertises SHA-256 and BLAKE3
  (`capabilities_server.rs:167-170`).

## 2. Bazel, the client

### From a spawn to an Action

`buildRemoteAction` (**read**, `lib/remote/RemoteExecutionService.java:511-590`):

1. The Merkle tree of the spawn's inputs (525-531).
2. Platform properties from the spawn, the platform and `--remote_default_exec_properties`, plus
   a `persistentWorkerKey` when tool inputs are marked (538-545).
3. The `Command` from argv, environment, outputs and working directory (549-559); outputs and
   environment are sorted (`buildCommand`, 270-331, environment at 316-323).
4. The `Action` from the command digest, the root digest, the platform, the timeout,
   `do_not_cache` when the spawn may not be cached remotely, and the salt (562-568,
   `lib/remote/util/Utils.java:430-452`).
5. The action key is the digest of the Action (570). `RequestMetadata` is built beside it, not
   in it.

### Remote cache, remote execution, disk cache

- A spawn reads the remote cache when `--remote_accept_cached` and the spawn allows; the disk
  cache when the spawn allows (**read**, `RemoteExecutionService.java:342-352`). It writes the
  remote cache when `--remote_upload_local_results` (default `true`,
  `RemoteOptions.java:333-337`) and the server takes AC updates (`RemoteExecutionService.java:354-365`).
  So a client that ran an action locally uploads its result to the shared AC unless told not to.
- Bazel's own advice: "Take care in who has the ability to write to the remote cache. You may want
  only your CI system to be able to write to the remote cache" (**read**,
  `site/en/remote/caching.md:232-233`); read-only clients set
  `--remote_upload_local_results=false` (`caching.md:244-247`). Deleting by build is not possible:
  "there's no way to distinguish which output belongs to a specific build"; a poisoned cache is
  cleaned by deleting (`caching.md:269-275`).
- tracedecay sets neither flag (**read**, `.github/actions/setup-bazel/action.yml:119-143`). It
  uses one instance, `main` (`action.yml:123`), for `master` and for pull requests from the same
  repository by the owner or the bot (`.github/workflows/ci.yml:346-349`). Its tests race a local
  branch on the hosted runner (`action.yml:133`, `139`). NativeLink can make an instance's AC
  read-only (`nativelink-config/src/cas_server.rs:115-125`, enforced in
  `nativelink-service/src/ac_server.rs:131-133`) but still advertises `update_enabled: true`
  (`capabilities_server.rs:171-173`). Whether the fleet's AC is read-only for CI clients is not in
  the repository (**unverified**).

### Build without the bytes

- `--remote_download_outputs` is `toplevel` by default; `minimal` downloads only what local
  actions need (**read**, `RemoteOptions.java:520-556`). tracedecay sets `toplevel`
  (`action.yml:128`). Client output fetching for the five critical actions totalled 0.005 s
  (**read**, the note line 37).
- An output then lives only remotely, and an evicted blob is a lost input. Bazel retries the
  build up to `--experimental_remote_cache_eviction_retries` times (default 5), each with a new
  invocation id (**read**, `lib/exec/ExecutionOptions.java:504-513`), and can extend leases by
  calling `FindMissingBlobs` periodically (`RemoteOptions.java:719-723`).
- buck2 does the same ("deferred materialization") and reports builds "approximately 2.5 times
  faster" at Meta (**reported**, buck2 `docs/users/advanced/deferred_materialization.md:8-17`).

### Dynamic execution and local fallback

- Dynamic execution starts the local and the remote branch of one action together, keeps the
  first to finish and cancels the other (**read**, `site/en/remote/dynamic.md:8-13`). A branch
  that finishes first with a failure fails the action, "an intentional choice to prevent
  differences between local and remote execution from going unnoticed" (`dynamic.md:68-70`). A
  cache-only remote cannot be raced (`dynamic.md:79-80`). After a remote cache hit has been seen,
  local starts are delayed by `--dynamic_local_execution_delay`, 1000 ms by default
  (`dynamic.md:44-52`, `lib/dynamic/DynamicExecutionOptions.java:102-110`).
- `--remote_local_fallback`, off by default, runs an action locally when remote execution fails
  (**read**, `RemoteOptions.java:307-311`).
- What tracedecay sets (**read**, `action.yml:119-143`):

  | Flag | Value | tracedecay's comment |
  |---|---|---|
  | `--strategy=Rustc` | `remote` | "Large Rust links raced on the hosted VM while the same actions ran remotely. local_resources only schedules estimates; it does not cap rustc memory." (135-137) |
  | `--strategy=TestRunner` | `dynamic`, local branch `sandboxed` | tests hybrid |
  | `--noremote_local_fallback` | | "Fleet compilation must not fall back onto the smaller hosted VM." (125) |
  | `--jobs` | 40 | |
  | `--local_resources` | `cpu=HOST_CPUS`, `memory=HOST_RAM*.5` | |
  | `--remote_timeout` | 600 | |
  | `--remote_download_outputs` | `toplevel` | |

  `docs/ci-remote-builds.md:14-16` still says Rustc races; the action file is what runs (**read**).

### Output path stripping

`--experimental_output_paths` (off by default) lets an action that advertises
`supports-path-mapping` drop the configuration segment, such as `k8-opt`, from its paths, so two
configurations share cache entries (**read**, `lib/analysis/config/CoreOptions.java:932-947`,
`lib/actions/PathMapper.java:31-40`). rules_rust advertises it on its Rustc actions when it can and
routes `OUT_DIR` through file arguments for it (`rust/private/rustc.bzl:1138-1153`, `1976`, `1994`,
`2016`). tracedecay does not set the flag (**read**, `.bazelrc`). Its measured actions wrote to
`bazel-out/k8-opt/` (**read**, the JSON's `out` fields).

### Input roots, and what 5.2 GB costs

- Bazel computes a spawn's root digest without building the whole tree when it can, keeps the
  digests of self-contained subtrees (tree artifacts, runfiles trees, source directories) between
  builds, and is written to keep memory flat with thousands of remote actions in flight
  (**read**, `lib/remote/merkletree/MerkleTreeComputer.java:102-138`). It keys those caches on
  Bazel's file metadata values, which "fully describe their contents" (line 127-129).
- Before `Execute`, Bazel asks `FindMissingBlobs` about the tree and uploads what is missing
  (**read**, `RemoteExecutionCache.java:380-460`).
- tracedecay's five critical Rust actions (**read**, the JSON and the note):

  | Crate | Declared input bytes | Files | Client upload | Worker setup, input fetch included | Execution | Output upload |
  |---|---|---|---|---|---|---|
  | tracedecay-agent-hosts | 5,196,362,702 | 33,206 | 0.441 s | 1.622 s | 21.898 s | 0.803 s |
  | tracedecay-project | 5,402,896,219 | 33,084 | 0.280 s | 1.989 s | 3.365 s | 0.105 s |
  | tracedecay-daemon-service | 5,416,401,347 | 33,201 | 0.284 s | 1.609 s | 55.307 s | 1.613 s |
  | tracedecay-mcp | 5,533,647,147 | 33,386 | 0.285 s | 39.689 s | 70.801 s | 31.318 s |
  | tracedecay | 5,663,475,740 | 33,591 | 0.264 s | 1.669 s | 200.846 s | 5.957 s |

  The declared size cost the client little. It costs the worker when its directory cache misses;
  that this is why tracedecay-mcp took 39.7 s is **unverified** (the record does not say). The
  note's own caveat: declared bytes "cannot establish CAS bandwidth" (line 37).
- Why every action declares so much: rules_rust puts the crate's sources, every transitive
  dependency's rlib (rmeta with pipelining), proc-macro data, `toolchain.all_files` and the C++
  linker files into each Rustc action (**read**, `rust/private/rustc.bzl:763-765`, `794-806`).
  `toolchain.all_files` is the whole sysroot (rustc, its libraries, std, LLVM tools, cargo,
  clippy, rustdoc, rustfmt) plus the C++ toolchain's `all_files`
  (`rust/private/toolchain.bzl:300-346`, `585-595`). tracedecay's Linux C++ toolchain is the zig
  SDK (`.bazelrc:110-113`, `MODULE.bazel:33-38`). How the 5.2 GB splits between toolchains and
  rlibs was not measured (**unverified**).

### Persistent and multiplex workers

- A persistent worker is a long-running process the Bazel server starts, wrapping a tool that can
  do a sequence of compilations; it gets its own working directory but is "not chrooted to" it
  (**read**, `site/en/remote/persistent.md:11-20`). A multiplex worker serves several requests in
  one process (`multiplex.md:13-16`). Under dynamic execution a worker runs sandboxed and cannot
  multiplex (`dynamic.md:55-58`).
- Remote persistent workers: Bazel marks tool inputs and sends a `persistentWorkerKey` platform
  property (**read**, `RemoteOptions.java:659-663`, `RemoteExecutionService.java:541-545`).
  NativeLink pools processes for actions with `supports-workers=1`, keyed by executable, startup
  arguments, environment and protocol; at most 4 per key, idle ones kept 300 s, each retired after
  200 requests "which bounds what a long-lived process accumulates"
  (`nativelink-config/src/cas_server.rs:1181-1230`). Its memory enforcement does not cover them,
  "since that process outlives the action and serves others" (`cas_server.rs:1440-1446`).
- rules_rust 0.74.0 registers no worker: no `supports-workers` or `supports-multiplex-workers` in
  any of its `.bzl` files (**read**, by grep). rustc under Bazel is a fresh process per action.

### Test results are cached

- `--cache_test_results=auto`, the default, reruns a test only if "Bazel detects changes in the
  test or its dependencies", it is tagged `external`, `--runs_per_test` asks for more runs, or it
  "previously failed" (**read**, `lib/analysis/test/TestConfiguration.java:169-183`). A previous
  failure always reruns, "as otherwise we can get stuck forever in the event of a flaky failure",
  and Bazel never uploads failures itself (`lib/analysis/test/TestRunnerAction.java:576-600`,
  `650-663`).
- `--flaky_test_attempts=default` runs a test once, or three times when its rule says `flaky=1`,
  and marks a test that needed more than one attempt FLAKY (**read**,
  `lib/exec/ExecutionOptions.java:240-252`).
- The key is the test action's digest: the test program, its declared runfiles, arguments,
  environment and platform. Time, randomness and thread scheduling are not in it (**unverified**,
  inferred from what an Action holds). tracedecay's `rust_test` declares cargo, rustc, rustdoc and
  rust-analyzer's sources as runfiles, gives each test a private `HOME`, `CARGO_HOME` and
  `RUSTUP_HOME`, and reserves `cpu_count=4` (**read**, `scripts/bazel/rust_test.bzl:10-31`,
  `98-126`, `268`).

## 3. rules_rust 0.74.0

### One crate, its actions

- A `rust_library` is one `Rustc` action: `process_wrapper` running rustc, with the crate's
  inputs, outputs, environment and a `resource_set` (**read**, `rust/private/rustc.bzl:1958-1977`).
- With `pipelined_compilation`, off by default (`rust/settings/settings.bzl:105-115`), a second
  action, `RustcMetadata`, runs the same rustc with `--rustc-quit-on-rmeta`, and `process_wrapper`
  ends rustc once the `.rmeta` is written (`rustc.bzl:1224-1228`, `1978-1995`,
  `util/process_wrapper/main.rs:113-114`). Dependents of a library then take its `.rmeta`
  instead of its `.rlib` (`rustc.bzl:648-668`, `763-765`). The command lines of both must match
  or rustc rejects the rmeta, so both emit metadata (`rustc.bzl:1785-1795`). tracedecay does not
  enable it (**read**, `.bazelrc`).
- A build script is its own action, `CargoBuildScriptRun`, whose `OUT_DIR` is a tree artifact
  (**read**, `cargo/private/cargo_build_script.bzl:410`, `720`); its environment and flag files
  become inputs of the crate's Rustc action (`rustc.bzl:848-852`).
- A proc-macro crate is a Rustc action of its own, linked like a binary (`rustc.bzl:749`, `811`),
  and its data is an input of every crate that uses it (`rustc.bzl:801`) (**read**).
- `process_wrapper` substitutes paths (`--subst`), reads environment and argument files from
  build scripts, captures output, and stops rustc after the rmeta (**read**,
  `util/process_wrapper/options.rs:41-93`).
- **No incremental.** Nothing in `rust/private` or `rust/settings` passes `-Cincremental`; the one
  match for "incremental" is a comment about LTO (**read**, grep). buck2 makes incremental Rust
  actions local-only (**read**, buck2 `prelude/rust/build.bzl:2044`); sccache requires
  incremental off (`docs/Rust.md:10`).
- **Codegen units.** `//rust/settings:codegen_units` defaults to -1, which passes nothing and
  leaves rustc's default (**read**, `settings.bzl:556-567`). When set, it also sizes the action's
  local CPU reservation (`rust/private/rustc_resource_set.bzl:262-296`). A `resource_set` steers
  Bazel's local scheduler only. On the fleet the reservation is the `cpu_count` exec property:
  `1` for every tracedecay action, `4` for tests (`scripts/bazel/platforms/BUILD.bazel:9-13`,
  `rust_test.bzl:268`).

### What changes a crate's action key

All **read** from the input set and command above, unless marked:

- any byte of the crate's sources or compile data;
- any byte of any transitive dependency's rlib, or rmeta with pipelining;
- any file of the Rust sysroot or the C++ toolchain;
- any rustc flag, including the per-crate `-Copt-level` lines tracedecay generates
  (`.bazelrc:90-102`) and `extra_rustc_flag`s (`.bazelrc:38`, `42`);
- `--action_env` values (`.bazelrc:24`, `34-37`);
- a build script's outputs;
- the platform properties and the timeout.

There is no early cutoff except when a dependency's output bytes come out identical. A body edit
changes the rlib's code. Whether it leaves the rmeta byte-identical depends on what rustc hashes
into it and was not checked (**unverified**).

### Why the `tracedecay` crate is one 200 s action

- rules_rust registers one Rustc action per crate (above). REAPI runs an action on one worker
  (`re.proto:731-734`) and has nothing that splits one (**read**).
- Inside the action the parallelism is rustc's own: LLVM codegen units, and a front end that is
  serial unless `-Zthreads` is passed (**unverified** for this nightly; `.bazelrc` passes no
  `-Zthreads`, **read**).
- The action asked for `cpu_count=1`, so nothing kept a large machine for the longest pole
  (**read**, `platforms/BUILD.bazel:12`). It had 226 source files, queued 10.504 s and executed
  200.846 s (**read**, the JSON's action name, the note line 25).
- Under Bazel the levers are splitting the crate in source, pipelining (dependents start sooner;
  the crate itself is not faster) or a faster rustc. That blaze spreads one crate over every core
  of a slot is the design's claim (distributed.md line 432); that it removes this pole for
  tracedecay is **unverified** until G0 measures blaze compiling that crate.

## 4. NativeLink

### Architecture

One binary configured by one JSON5 file: named `stores`, `schedulers`, `servers` (each a
listener with `services`: `cas`, `ac`, `capabilities`, `execution`, `bytestream`, `fetch`,
`push`, `worker_api`, `experimental_bep`) and `workers` (**read**,
`nativelink-config/src/cas_server.rs:514-583`; examples in `nativelink-config/examples/*.json5`).
Workers register with the scheduler over the worker API. tracedecay's coordinator listens on 50051
for clients and 50061 for its three fixed workers (**read**, `docs/ci-remote-builds.md:47-51`).

### Stores

| Store | What it does (**read**) | Source (`nativelink-config/src/stores.rs`) |
|---|---|---|
| `memory` | a hash map in memory | 75 |
| `filesystem` | persistent across restarts | 495-511 |
| `fast_slow` | read `fast`, else `slow` and copy into `fast`; uploads go to both. "this store will never check to see if the objects exist in the `slow` store if it exists in the `fast` store" | 430-469 (warning 438-443) |
| `verify` | checks hash and size of uploads; suggested: the CAS checks both, the AC nothing | 258-282 |
| `completeness_checking` | AC only: returns an ActionResult only if its outputs exist in the CAS | 284-309 |
| `compression` | lz4 in and out of the backend | 311-336 |
| `dedup` | content-defined chunks in a `content_store`, an index in an `index_store`; not for compressed content; `has()` checks only the index | 338-403 |
| `existence_cache` | caches `has` answers; CAS only | 405-428 |
| `size_partitioning` | by the digest's size; CAS only | 528-554 |
| `shard` | by hash, weighted | 471-493 |
| `grpc`, `redis`, cloud objects (S3, GCS, Azure, R2, OCI), `ref_store`, `noop` | backends and plumbing | 233, 526, 588, 612, 624 |

Eviction is per store: `max_bytes` with an `evict_bytes` low-water mark, `max_seconds` since last
access, `max_count` (**read**, `stores.rs:1199-1221`). Nothing evicts by reachability from a build:
an AC entry and the blobs it names are evicted independently, which is what
`completeness_checking` and Bazel's eviction retries exist for (**unverified** that this is the
whole reason; the sources give each mechanism, not the history).

### How the scheduler matches actions to workers

- A platform property is `minimum` (a number: the worker must have at least that much), `exact`
  (string equality), `priority` (informational) or `ignore` (**read**,
  `nativelink-config/src/schedulers.rs:36-64`). A `minimum` is subtracted from the worker's
  remaining amount while the action runs and given back after (**read**,
  `nativelink-scheduler/src/worker.rs:205-222`, `483-495`). A worker may also cap
  `max_inflight_tasks`, 0 meaning no cap (`cas_server.rs:1384-1388`, `worker.rs:496-502`).
- Among workers that fit: least recently used, most recently used, `LeastLoaded`, or `BestFit`
  (the worker the action fits most tightly by its `minimum` properties) (**read**,
  `schedulers.rs:69-95`).
- The queue is ordered by priority, then by insert time (**read**,
  `nativelink-scheduler/src/awaited_action_db/awaited_action.rs:185-186`, `288-312`).
- Retries: `max_job_retries`, `max_worker_loss_retries`, worker and action timeouts (**read**,
  `schedulers.rs:183-319`).
- **Identity.** A cacheable action already queued or running is joined, not run again; each
  client keeps its own operation id and the execution counts its connected clients; an
  uncacheable action never merges (**read**,
  `nativelink-scheduler/src/memory_awaited_action_db.rs:752-800`, `843-900`;
  `nativelink-util/src/action_messages.rs:184-190`). Each update carries a version and a stale one
  is refused with `ABORTED` (`memory_awaited_action_db.rs:600-635`).
- tracedecay's fleet config (worker capacities, property types, `max_inflight_tasks`, allocation
  strategy) is not in its repository (**read**: `.bazelrc:108-109` says "Private endpoints and
  capacity stay outside the repo"). If `cpu_count` is `minimum` and a worker advertises its core
  count, a worker of N cores runs up to N Rust actions at once however many threads each rustc
  uses (**unverified** inference).

### Worker execution and input materialization

- The worker's CAS must be a `fast_slow` store whose fast tier is a filesystem store; files are
  hardlinked from it into the action's directory, which must be on the same filesystem (**read**,
  `cas_server.rs:1515-1535`).
- An optional directory cache keeps materialized input roots, and experimentally every subtree by
  its own digest, for reuse by hardlink; 1000 entries and 10 GB by default (**read**,
  `cas_server.rs:1557-1562`, `1678-1710`). Materialized files are made read-only, `0o444` or
  `0o555` (`nativelink-worker/src/directory_cache.rs:176`, `1192-1223`). Downloads run 64 at a time
  per directory level (`nativelink-worker/src/running_actions_manager.rs:1051`).
- Outputs are hashed, uploaded to the CAS, and the ActionResult to the AC; by default only results
  with exit code 0 (`SuccessOnly`) (**read**, `cas_server.rs:1119-1132`).
- **Isolation is optional.** Linux PID and mount namespaces and a private `/tmp`
  (`cas_server.rs:1617-1660`), an `entrypoint` wrapping every command (`1453-1459`), and
  `single_use`: "Accept one action, upload its results, and exit the worker process ... Reusing
  the container or its writable volumes does not provide isolation from the previous action"
  (`1390-1400`) (**read**). Its read-only input mounts "are not a security boundary", and "An
  action can remount its inputs writable in its own user namespace and corrupt the shared cache"
  (`cas_server.rs:1594-1607`) (**read**).

### Action-cache poisoning protections

What exists (**read**): `verify` on CAS writes; `completeness_checking` on AC reads; a per-instance
read-only AC; `SuccessOnly` uploads; REAPI's `salt` to disown a set of results and
`skip_cache_lookup` to overwrite a poisoned one (`re.proto:721-729`, `1592-1605`).

What does not: anything that ties an ActionResult to having run its Action. REAPI has no
signature field on a result (**read**, `re.proto:1250-1416`). An AC entry is trusted because who
could write it was restricted (**unverified** as a description of intent; it is what the fields
allow).

### Auth

- TLS, with optional client certificates (a CA and a revocation list) (**read**,
  `cas_server.rs:596-606`).
- An identity header names a client and is not verified (**read**, `cas_server.rs:476-484`,
  `752-754`).
- Bearer tokens and JWTs are forwarded to upstream gRPC stores and schedulers, not checked
  (**read**, `stores.rs:579-583`, `1663-1670`, `schedulers.rs:449`). No OIDC verification exists
  (**read**, grep for `jwt`, `oidc`, `bearer`).
- tracedecay connects in plaintext, `grpc://ubuntu-main:50051` (**read**, `action.yml:121-122`).
  Authorisation is the tailnet: a Tailscale OIDC client bound to repository, actor and workflow
  ref, an ephemeral node tagged `tag:bazel-ci`, and a grant to 50051, 1985 and 443 only
  (`docs/ci-remote-builds.md:27-52`).

## 5. Where tracedecay's time goes

One interrupted diagnostic run of a release CLI build (run 39830079562, Bazel 9.3.1), no repeats,
no worker CPU, memory, disk or network counters (**read**, the note lines 3, 9, 75). 2,503 of
2,515 completed spawns were remote cache hits (line 11). The costs of the five uncached critical
Rust actions and of the run:

| Cost | Measured (**read**, the note) | What causes it | Would blaze have it? |
|---|---|---|---|
| Queueing | 479.395 s of 924.604 s, 51.8% (line 3); per action 10.5 to 178.9 s (19-25) | NativeLink's dispatch; the limiter is not identified: "eligibility, admission, capacity, or fairness" are unknown (67) | Yes, whenever jobs outnumber warm slots. A job holds a slot longer than an action holds a CPU, so slot count is the new limit (**unverified**; G3 and a burst test decide) |
| Worker setup and input fetch | 46.578 s total; 39.7 s for one action (37, JSON) | materializing a 5.2 GB, 33,000-file input root per action; the directory cache helps when the root repeats | Much less: a slot keeps its worktree and toolchain and fetches a commit and missing packs (**unverified** until G2) |
| Execution | 352.217 s; `tracedecay` alone 200.846 s (3) | rustc, one crate per action | blaze's own compile; the crate spread over a slot's cores (design claim, **unverified** for tracedecay) |
| Output upload | 39.796 s total; 31.318 s for tracedecay-mcp (24, 37) | hashing and uploading the rlib and debug outputs | Yes, as publishing packs; the size per job is not known (**unverified**) |
| Client input upload | 1.554 s total (37) | Bazel's `FindMissingBlobs` and uploads | Only the job request and a source ref |
| Client output fetch | 0.005 s (37) | `toplevel` downloads | Only programs the requester runs |
| Repository fetch before execution | 487.687 s in `git submodule update` for `tokensave-large-treesitters` during analysis (5, 53) | crate_universe's repository rule on the hosted runner | blaze fetches git dependencies and their submodules itself (`blaze-registry/src/git.rs`, distributed.md line 623); a slot pays it once, a cold runner each time unless cached (**unverified**; G2 measures) |

The run built `k8-opt`, the release configuration, which blaze's design keeps on Bazel
(distributed.md section 7). It shows the shape of the costs, not a baseline.

## 6. Rust prior art beside Bazel

### sccache and sccache-dist

- sccache caches rustc invocations that cargo makes. Its key hashes a version, the compiler's
  shared libraries, the full command line, every source file from dep-info, the `--extern` files,
  static libraries, the target JSON, the environment variables dep-info lists, the working
  directory and the compiler version (**read**, `src/compiler/rust.rs:1805-1906`). The working
  directory "will wind up in the rlib" (1904), so a hit needs the same checkout path.
- It cannot cache crates that run the linker (`bin`, `dylib`, `cdylib`, `proc-macro`), needs
  incremental off, and may cache proc macros that read files wrongly (**read**, `docs/Rust.md:9-11`).
- sccache-dist is a client, a scheduler and build servers; servers are 64-bit Linux or FreeBSD only.
  Linux clients package their compiler automatically; macOS and Windows clients must name a cross
  toolchain (**read**, `docs/Distributed.md:18-32`). Builds run under bubblewrap or Docker
  (`docs/Distributed.md:368-398`). A proc-macro dylib input cannot be shipped from a platform that
  cannot distribute dylibs (`src/compiler/rust.rs:2508-2514`). Servers authenticate to the scheduler
  by JWT or a shared token; clients by OAuth2 (`docs/Distributed.md:64-210`).
- Its unit is Bazel's: one rustc invocation. A long crate stays one job.

### buck2

- Rust rules over REAPI; pipelining emits a "hollow" rlib with `-Zno-codegen`, nightly only
  (**read**, `prelude/rust/build.bzl:1833-1850`); incremental actions run local-only (`2044`).
  Deferred materialization as above.

### cargo

- A per-user compiled artifact cache is an open issue, rust-lang/cargo#5931 (**read**, `gh issue
  view`, open, updated 2026-10-06). [rust-compiler-2026.md](rust-compiler-2026.md) covers cargo's
  build-dir.

## 7. Each mechanism against blaze

blaze's side is **read** at `c8e5f6a8` and in the design at `45449ae6` unless marked.

### 7.1 Content-addressed input roots against blaze's Merkle VFS roots

- **Bazel.** Every action's key covers its whole declared input tree. An edit anywhere in a
  crate's closure changes the key of every action downstream; the cache hit is all or nothing per
  action (section 2, 3).
- **blaze.** A function's key is its stable name with the settings that change codegen
  ([cache.md](../cache.md), `crates/ra-jit-store/src/lib.rs:173-197`). The design rejects a tree
  hash as the cache key (distributed.md line 325) and plans the Merkle root (#271) only as
  `SourceRef::Root`, a name for the tree a job builds.
- **Verdict: reject as a cache key, adapt as transport.** A per-function key already gives what
  output path stripping and early cutoff try to recover. A Merkle root is the right way to name a
  job's tree and a test's runtime files (7.10), where "the same bytes" is the question.

### 7.2 One ActionCache entry per action against per-crate records

- **Bazel/NativeLink.** One mutable map, action digest to ActionResult, read one digest a call
  (`re.proto:170`); for Rust, one entry per crate per configuration.
- **blaze.** One signed `CrateRecord` per `CrateKey` per namespace, looked up in one batch
  (distributed.md lines 179-186, 357-376).
- **Verdict: adapt; keep per-crate records.** The per-crate record answers "which packs hold this
  crate's functions", which no action answers in blaze because there is no per-crate action. Take
  from the AC: the server checks a record's packs exist before returning it (`completeness_checking`),
  and only successful jobs publish (`SuccessOnly`).

### 7.3 FindMissingBlobs against pack manifests

- **Bazel.** Ask which digests are missing, upload only those, cache "present" answers
  (section 1, 2).
- **blaze.** Today a push sends the packs the project manifest lacks (`crates/ra-jit/src/remote.rs:7-9`);
  the design adds `POST /v1/cache/missing` (line 559).
- **Verdict: adopt.** Add NativeLink's `existence_cache` idea inside `ra-jit serve`: answer
  `missing` from an in-memory set of what the server holds.

### 7.4 Dynamic execution against blaze's local fallback

- **Bazel.** Per action, race local and remote; the first failure fails; tracedecay turned racing
  off for Rustc because large links on the hosted VM hurt, and turned fallback off (section 2).
- **blaze.** `LocalPolicy { Never, Fallback, Race }`, racing per job (distributed.md lines 223, 497-503).
- **Verdict: adapt.** Race per job, never per function. Keep Bazel's rule that a first-finishing
  failure fails the job. A successful build on the other executor cannot hide that
  failure. Default `Never` on hosted runners, with tracedecay's reason as the evidence.

### 7.5 Persistent workers against warm slots

- **Bazel/NativeLink.** A worker is keyed by its tool (executable, arguments, environment), capped
  per key, evicted when idle, retired after 200 requests to bound accumulated state, and not
  isolated from the actions it serves (section 2).
- **blaze.** A slot is a worktree with a daemon warm on `master`, keyed by `WorkspaceKey`, rebuilt on
  identity, lock-file, memory or daily triggers (distributed.md lines 436-469).
- **Verdict: adapt.** A warm slot is a multiplex persistent worker for a whole workspace. Take
  retire-after-N-jobs and idle eviction as rebuild triggers beside memory. Take NativeLink's warning
  as a design input: a process that outlives jobs is not a trust boundary, so a slot that ran a
  pull request does not publish to `Main` (7.8).

### 7.6 fast_slow, verify and completeness stores against `ra-jit serve`

- **NativeLink.** Composable stores: tiers, verification on write, completeness on AC read,
  existence caching, eviction per store (section 4).
- **blaze.** The local store in front of a remote place, verified reads by digest
  (`remote.rs:19-21`), the keeper pushing and pulling (`remote/keeper.rs:1-6`); the design's server
  over the `Dir` layout (distributed.md lines 336-355).
- **Verdict: adopt the behaviours, reject the composition.** `ra-jit serve` needs: verify every
  blob before answering (the design has it), drop from a lookup answer any pack it lacks
  (completeness), an existence set for `missing`, and a byte budget. A JSON5 graph of stores is
  for a general service; one fleet needs one layout. Dedup, compression and size partitioning add
  nothing for zstd packs on one server (NativeLink's own dedup note: not for compressed content,
  `stores.rs:338-403`).

### 7.7 Platform properties against the target triple in blaze's identity

- **REAPI/NativeLink.** Platform properties are in the action key and drive matching: `exact` for
  OS and architecture, `minimum` for CPU and memory (section 1, 4). tracedecay's `cpu_count` hint is
  therefore in every action key (**unverified** consequence). The worker's undeclared environment is
  not in the key at all (`re.proto:746-748`); tracedecay closes that on Linux by declaring the zig C
  toolchain as an input (`.bazelrc:112`), and its Mac's newer SDK is why hosted macOS stays hosted
  (`docs/ci-remote-builds.md:10-12`).
- **blaze.** The target triple is in every entry key and the build's `TARGET` in the identity
  (`crates/ra-jit/build.rs`, distributed.md lines 296-302); `SlotState.host_triple` picks a slot.
- **Verdict: adapt.** Keep what changes output in keys (triple, identity), and keep what only
  schedules (CPU, memory) out of every key. Add a digest of the native linker and SDK where the
  output depends on them (review-b M5), as tracedecay does by declaring its Linux toolchain.

### 7.8 Action-cache poisoning against blaze's signed records

- **NativeLink today, for tracedecay.** One instance for `master` and pull requests; workers that
  run pull-request tests also compile `master`'s actions with a shared hardlinked input cache;
  clients upload local results by default; trust is by tailnet (sections 2, 4). A malicious test on
  a worker or a runner could write a well-formed AC entry for a real action digest
  (**unverified**: no attack was tried; each step is allowed by the sources).
- **blaze.** Packs verified by digest; manifests signed with turns (`remote/signed.rs:1-13`); the
  design signs records on the server, per namespace, and lets `Main` be written only by `master`'s
  builds (distributed.md lines 378-410).
- **Verdict: adopt the namespace and signing, fix the slot.** Signing proves who published, not
  what ran. Both reviews show a slot that ran a pull request can carry its state into a `Main`
  publication. NativeLink shows the same hole and its own fix: `single_use` workers. Add REAPI's
  two escape hatches: an epoch in every signed record (the `salt`), so a namespace can be disowned
  at once, and a forced rebuild of one crate that republishes with a higher turn
  (`skip_cache_lookup`).

### 7.9 Should blaze speak REAPI to NativeLink during the migration?

#226 proposes REAPI as one more remote `Place`: packs as CAS blobs, function keys and manifests as
AC entries, signatures inside the blobs (**read**, issue text).

- **For.** NativeLink stays up for what Bazel keeps (distributed.md section 7); one cache server
  for both would avoid a second service during shadow mode. Integrity holds: blobs are checked by
  digest and manifests by signature, whatever the server is (#226).
- **Against.**
  - No batch AC read (`re.proto:170`): a build's 500 crate records are 500 calls, the cost the
    design's batch lookup exists to avoid (distributed.md lines 340-343).
  - The AC cannot carry blaze's namespaces or its grants: NativeLink verifies no OIDC token, and
    its read-only switch is per instance, not per caller (section 4).
  - NativeLink evicts AC entries and blobs independently by LRU; blaze's GC by live roots cannot run
    on it.
  - `ra-jit serve` is needed anyway as the job coordinator (distributed.md lines 336-338), so a
    REAPI client saves no service in the end state.
  - It adds a gRPC stack, which the standing orders allow only as an edge adapter.
- **Verdict: reject for this program.** Keep #226 as an optional, cache-only edge for teams that
  run NativeLink or BuildBuddy, after unit 5's per-crate records exist, so the mapping is records
  to AC entries and not today's project manifests. Nothing blaze compiles can use NativeLink's
  existing entries: they are rustc's outputs.

### 7.10 Test results

- **Bazel.** Passes cached under the test action's digest (declared runfiles, environment,
  arguments, platform); reruns on `external`, `--runs_per_test`, a previous failure; never caches
  failures; three attempts for `flaky=1` (section 2).
- **blaze.** `TestKey` from the reach hash, attributes, fixed environment, `CARGO_BIN_EXE_*`
  contents and the digests of every path the sandbox recorded; passes skipped by key; two verdicts
  under one key is a flake (distributed.md lines 242-254, 511-534).
- **Verdict: adapt.** Bazel shows the industry accepts a cached pass under a complete declared-input
  key, with an opt-out tag and failures never cached. blaze's key is recorded, not declared, so it
  also needs what review-b M6 asks: the recorded read set published with the verdict, and a
  runtime bundle (program, support tools, fixtures, environment) named by digest, which is Bazel's
  runfiles tree. Add an `external`-like attribute that always runs, and an audit mode that ignores
  the cache. tracedecay's own nextest policy fails on flakes (review-b M10); the design's "an
  unchanged flaky test does not fail" is a policy change for the owner to accept, not a default.

### 7.11 Job identity, deduplication and fencing

- **NativeLink.** Content key for merging, a server operation id per execution, a client operation
  id per caller, a connected-client count, versioned updates (section 4).
- **blaze.** One job digest names the content, the execution and the cancellation
  (distributed.md lines 287-288, 566-571).
- **Verdict: adopt.** Split them as NativeLink does (review-a M6). Cancel an execution when its last
  subscriber leaves, not when one connection closes. Fence a reassigned attempt's late events by
  version.

### 7.12 Scheduling and admission

- **NativeLink.** `minimum` properties subtracted while running, `max_inflight_tasks`, priority then
  FIFO, `BestFit` or `LeastLoaded`, retries counted, `ExecutedActionMetadata` timestamps per stage
  (sections 1, 4).
- **blaze.** "As many slots as its memory holds"; least diff cost, then least loaded; heartbeats and
  one reassignment (distributed.md lines 436-438, 486-509).
- **Verdict: adopt.** Reserve measured memory and CPU per slot, job and test shard, and subtract
  them while running (review-b M2). Order `Main` before pull requests, then FIFO. Report the stage
  timestamps REAPI has (queued, assigned, fetch, build, publish, test) on every job, so a queue
  always has a named limiter: tracedecay's measurement could not name one because the dispatch
  records were missing (the note line 67).

## 8. The reviewers' open questions, against this evidence

| Finding | What the evidence says |
|---|---|
| a-B2, b-B1: a pull request and `Main` share a slot's mutable state | NativeLink has the same mixing for tracedecay today (7.8). Its fix is `single_use` workers and namespaces; read-only inputs are not a boundary by its own docs (`cas_server.rs:1390-1400`, `1594-1607`). Supports the reviewers' fix: trusted `Main` slots that never run pull-request code, disposable pull-request state |
| a-M1, b-M1: capacity, not width; slots may queue worse | NativeLink admits per action by `minimum` CPU; tracedecay asks 1 CPU per Rust action and 4 per test (section 4). The fleet's capacities are not in the repository. A slot holds a machine-sized share for a whole job (**unverified** effect on queues). A burst test against NativeLink in one session is the only evidence that settles it |
| a-M6: job digest as execution id | NativeLink's split (7.11) |
| a-M7, b-M7: codegen shards lack completeness rules | No precedent: REAPI, Bazel, sccache-dist and buck2 all distribute whole rustc invocations; none splits a crate (sections 1, 3, 6). Mode D stays a gated experiment |
| b-B2: a passed `TestKey` is not a safe skip | Bazel caches passes under a declared-input key with the same blind spots, and limits the damage by an `external` tag, `--runs_per_test`, rerunning failures and never caching them (7.10) |
| b-M3: OIDC claims, fork policy | NativeLink verifies no OIDC; tracedecay's identity check is Tailscale's, bound to repository, actor and workflow ref (`docs/ci-remote-builds.md:29-45`), and fork exclusion is the workflow's `head.repo.full_name` test (`ci.yml:346`). blaze's server would be the first OIDC verifier in the path |
| b-M4: `WorkspaceKey` and `Want` cannot express cargo's flags | REAPI's answer is to key the resolved command: argv, sorted environment, working directory, outputs (`re.proto:749-900`). blaze's job needs the same: a resolved build spec, every output-changing input named |
| b-M5: native toolchain and SDK not in identity | REAPI leaves the undeclared environment out of the key (`re.proto:746-748`); tracedecay makes its Linux C toolchain a declared input (`.bazelrc:112`) and keeps the Mac out of hosted macOS for SDK skew (7.7) |
| b-M6: no read set, no runtime bundle for remote tests | Bazel's runfiles tree is that bundle; tracedecay's `rust_test` declares cargo, rustc and rust-analyzer in it (`rust_test.bzl:98-126`) |
| b-M8: tests that run rustc | tracedecay's tests get real rustc and cargo as runfiles on purpose (`rust_test.bzl:119-126`). Under blaze those tests need blaze-provided tools or stay on Bazel; Bazel is no help here |
| b-M9: crate-only GC roots pin every generic instance | NativeLink does no root GC: LRU per store, dangling AC entries caught by `completeness_checking`, lost inputs retried by Bazel (sections 2, 4). blaze can do better only by rooting the packs a build used (a record generation), with completeness checks at lookup and a lease for a running job |
| a-M2: historical numbers without spread | This document adds no timing beyond the note's single run, and labels it so |

## Adopt, adapt, reject

| # | Mechanism (where) | blaze today or in the design | Verdict | Why, for Rust only |
|---|---|---|---|---|
| 1 | CAS: blobs named by SHA-256, checked on transfer (REAPI) | packs named and checked by SHA-256 (`remote.rs:19-21`) | **adopt** (have) | already the same contract |
| 2 | Action digest over argv, environment, inputs, platform, timeout, salt (REAPI) | entry key per function; job digest; `TestKey` | **adapt** | name every output-changing input; never paths or scheduling hints |
| 3 | Whole input tree as one Merkle root in the key (REAPI, Bazel) | rejected as key (design line 325); `SourceRef::Root` planned | **reject** as key, **adapt** as transport | a tree key misses everything on any edit; a root is right for naming a job's tree and a test bundle |
| 4 | ActionCache, one entry per action, one read per call (REAPI) | signed per-crate records, batch lookup | **adapt** | blaze has no per-crate action; keep records, add completeness and success-only publishing |
| 5 | `FindMissingBlobs` before upload (REAPI, Bazel) | manifests; `/v1/cache/missing` planned | **adopt** | upload only what the server lacks |
| 6 | Small batch-blob calls (NativeLink 64 KiB) | packs already batch values | **reject** | a pack is the batch |
| 7 | ByteStream: resumable, compressed by name, digest of uncompressed bytes (REAPI) | `Range` resume (`transfer.rs`), `.zst` objects | **adopt** the shape | resumable, verified, compressed transfers |
| 8 | Digest-function negotiation (REAPI) | SHA-256 only | **reject** | one digest per schema version |
| 9 | `GetCapabilities` (REAPI) | `POST /v1/hello` | **adopt** (planned) | add explicit limits: request, blob, event sizes |
| 10 | Instance names as namespaces (REAPI) | `Main`, `Pr(n)`, `Std` | **adopt**, stronger | tracedecay shares `main` between `master` and pull requests |
| 11 | `salt` to disown a poisoned set (REAPI) | turns only | **adopt** | an epoch in signed records disowns a namespace at once |
| 12 | `skip_cache_lookup` to overwrite a poisoned entry (REAPI) | none | **adopt** | a forced rebuild of one crate that republishes with a higher turn |
| 13 | Clients upload local results by default; "only CI should write" (Bazel) | `Main` written only by `master` builds, server-signed | **adopt** the rule, **reject** the default | only trusted slots publish to `Main` |
| 14 | `verify` store (NativeLink) | server checks blobs before answering (planned) | **adopt** (planned) | |
| 15 | `completeness_checking` store (NativeLink) | lookup may name a pack that is gone | **adopt** | a record is answered with only the packs present |
| 16 | `fast_slow` tiers (NativeLink) | local store in front of the remote | **adopt** (have) | do not assume fast implies slow when publishing |
| 17 | `existence_cache` (NativeLink) | none | **adopt** | `missing` answered from memory |
| 18 | `dedup`, `compression`, `size_partitioning`, `shard` stores (NativeLink) | zstd packs, one server | **reject** | three machines, compressed packs |
| 19 | LRU eviction per store, independent AC and CAS (NativeLink) | GC by live roots (planned) | **adapt** | root the packs a build used, LRU the rest within a budget |
| 20 | Build without the bytes (Bazel, buck2) | whoever runs a program links it | **adopt** (have) | fetch only packs of programs the requester runs |
| 21 | Eviction retries and lease extension (Bazel) | none | **adapt** | a running job pins its record generations; no retry needed |
| 22 | Dynamic execution per action, first failure fails (Bazel) | `Race` per job (planned) | **adapt** | per job; keep first-failure-fails; `Never` on hosted runners |
| 23 | `--remote_local_fallback` (Bazel) | `Fallback` (planned) | **adopt** | off for hosted runners, as tracedecay chose |
| 24 | Output path stripping (Bazel) | keys hold no output paths | **reject** | not needed; #105's items are the remaining path dependence |
| 25 | Persistent and multiplex workers, remote pool keyed by tool, retire after N (Bazel, NativeLink) | warm slots by `WorkspaceKey` | **adapt** | add retire-after-N jobs and idle eviction; not a trust boundary |
| 26 | Directory cache of hardlinked read-only inputs (NativeLink) | a slot's persistent worktree | **reject** for compile, **adapt** for test bundles | a slot needs no per-job tree; test helpers do |
| 27 | `minimum` properties subtracted while running, `max_inflight_tasks` (NativeLink) | "as many slots as memory holds" | **adopt** | reserve measured memory and CPU per slot, job and test shard |
| 28 | Queue by priority then FIFO; `BestFit`, `LeastLoaded` (NativeLink) | least diff cost, least loaded | **adopt** | `Main` first; fit by memory |
| 29 | In-flight merge, client operation ids, versioned updates (NativeLink) | job digest is the id | **adopt** | content id, execution id, attempt version |
| 30 | `ExecutedActionMetadata` stage timestamps (REAPI) | `JobEvent::Stage` | **adopt** | fixed stage names, so every queue has a named limiter |
| 31 | Test results cached by key; `external`; failures never cached; flaky attempts (Bazel) | `TestKey`, flake rule | **adapt** | add an always-run attribute, audit runs, a runtime bundle by digest; flake policy is the owner's |
| 32 | Auth by mTLS or network; unverified identity header (NativeLink) | OIDC, server-issued grants, tailnet | **adapt** | keep the tailnet as the first gate, verify OIDC, issue per-job capabilities |
| 33 | `single_use` workers, namespaces (NativeLink) | sandbox per test; no per-job isolation | **adopt** for pull-request jobs | disposable state for untrusted code |
| 34 | Per-rustc-invocation distribution (Bazel, sccache-dist, buck2) | job on a warm slot | **reject** | blaze never runs rustc and has no per-crate action |
| 35 | REAPI as the wire to NativeLink during migration (#226) | CBOR over HTTP | **reject** for this program | no batch AC read, no namespace grants, no root GC; the server is needed anyway |

## What this changes in docs/distributed.md

1. **Trust.** In "Auth and trust", state that NativeLink today mixes `master` and pull requests in
   one instance on shared, persistent workers, with client AC uploads on by default (7.8), and that
   blaze does not copy it: `Main` is published only by slots that have never run pull-request code;
   pull-request jobs run on disposable state, as NativeLink's `single_use` does. Answers a-B2 and
   b-B1.
2. **Job identity.** Split the job digest (content, for merging) from an execution id (answered by
   `POST /v1/jobs`, used by events and `DELETE`) and an attempt version (fencing late events), as
   NativeLink does (7.11). Cancel when the last subscriber leaves. Answers a-M6.
3. **Admission.** Replace "as many slots as its memory holds" with reservations: each slot, job and
   test shard reserves measured memory and CPU, subtracted while running (NativeLink's `minimum`);
   `Main` before pull requests, then FIFO; stage timestamps on every job (REAPI's
   `ExecutedActionMetadata`). Keep CPU and memory out of every key. Answers b-M1 and b-M2 in part;
   the burst test against NativeLink stays a gate.
4. **Tests.** In section 4, cite Bazel's `cache_test_results=auto` as the precedent and its limits;
   add an always-run attribute, an audit mode, failures never cached, the recorded read set
   published with the verdict, and a runtime bundle by digest for helper slots. Make the
   unchanged-flake exemption the owner's decision. Answers b-B2, b-M6 and b-M10 in part.
5. **GC.** Root the packs a build used (a record generation), not crate keys; answer lookups with
   only the packs present (`completeness_checking`); give a running job a lease instead of Bazel's
   eviction retries. Answers b-M9.
6. **Records.** Add an epoch to the signed bytes of `CrateRecord` (REAPI's `salt`) and a forced
   rebuild of one crate (`skip_cache_lookup`).
7. **What tracedecay runs today.** Add: every Rust action carries `cpu_count=1` (tests 4) in its key;
   one instance `main` serves `master` and same-repository pull requests; clients upload local
   results unless the fleet's AC is read-only (unknown); auth is the tailnet, in plaintext gRPC;
   each Rust action declares 5.2 to 5.66 GB and about 33,000 files, most likely the sysroot and the
   zig SDK (**unverified**).
8. **Section 6.** Mark "that pole does not exist in the same shape" **unverified** until G0, and say
   the measured run was a `k8-opt` release build outside the scope.
9. **REAPI.** Replace "REAPI only as the edge adapter #226 describes, not planned here" with the
   verdict and reasons of 7.9.
10. **The job request.** Replace `WorkspaceKey` and `Want` with a resolved build spec in the sense of
    REAPI's `Command` (every argv, environment, feature, profile and working-directory input that
    changes output), plus a digest of the native linker and SDK where outputs depend on them.
    Answers b-M4 and b-M5 in part.
11. **Racing.** In "Failure", keep Bazel's rule that a first-finishing failure fails the job, and give
    tracedecay's reason for `Never` on hosted runners.
12. **Mode D.** Say plainly that no prior system splits one crate across machines; it stays gated.

## Gaps and not checked

- No NativeLink, Bazel or blaze code was run. Every behaviour above is from source text.
- The fleet's NativeLink version and config (property types, worker capacities,
  `max_inflight_tasks`, AC read-only, compression, directory cache) are not in tracedecay's
  repository, so which section 4 behaviours apply there is **unverified**.
- How the 5.2 GB of declared inputs splits between the Rust sysroot, the zig SDK and rlibs was not
  measured.
- Whether a body edit leaves rustc's rmeta byte-identical was not checked.
- Bazel's remote-execution path for tests (`TestRunner` remote branch) was read only where it shares
  code with spawns; `lib/remote/RemoteSpawnCache.java` was not read line by line.
- Review b may have grown after 22:01; this document answers what was there.
- The timebox did not allow fetching the fleet's dispatch records, which the note names as the
  next step for the queue (line 69); that remains the owner's.
