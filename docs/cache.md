# The disk store and the remote

ra-jit keeps compiled functions in memory while a daemon runs. Two more layers keep them
longer. The disk store is one directory on this machine that every daemon and every
checkout of the user shares, on by default. The remote is a directory or an S3 bucket that
several machines push to and pull from, off until you configure it.

The reports behind this file are [reports/DISKCACHE.md](reports/DISKCACHE.md) and
[reports/REMOTE.md](reports/REMOTE.md). The variables and commands below were checked
against `crates/ra-jit-store/src/lib.rs` and `crates/ra-jit/src/remote.rs`.

## What is stored and under what key

The store holds compiled functions: machine code with its relocations, unwind information
and line tables. It does not hold rust-analyzer's analysis, which is rebuilt when a daemon
loads a workspace.

A function's key is a SHA-256 over the backend and optimisation level, the target triple,
whether the build has debug info and overflow checks, and the function's stable name. The
stable name holds the function's path, its generic arguments spelled out, and its crate. A
type whose spelling would be longer than 1 KB is written as the SHA-256 of that spelling
instead: a closure that wraps closures holds each of their types twice, so written out in
full its name doubles with each level (helix's parser combinators made one of 665 MB). A
dependency is named by rustc's stable crate id (name, version, and the metadata cargo
passes). A crate of your checkout is named by a hash of its source, so two checkouts with
the same source share entries wherever they are on disk. An edit gives new names to
everything in the edited crate and in the crates of the checkout that depend on it, so
what the store serves across edits is the code of crates the edit does not reach:
dependencies, their generic instances, and the untouched crates of the workspace. Within
one running daemon, unchanged functions of an edited crate are kept by the database
instead.

An entry of the checkout also holds its mark: a hash of its code and of what it calls and
reads, by the same stable names, with the crates of the checkout named as they were when
the daemon took it. The base an edit is patched against compares the mark of the code now
with it, so a daemon that starts on a store holding the checkout's code takes that code as
it is, and the first edit recompiles only what it touched. Before, those marks hashed ids
of one process, and a started daemon compiled all it took of the checkout again before the
first edit could be a patch (nushell: 73,586 functions, 9.4 s). A program that links
rustc's code beside ra-jit's, an LLVM build and `#[ra_jit::cache]` functions keep no mark
and still take that build ([#105](https://github.com/indexable-inc/ra-jit/issues/105)).

Before a record is written, the store hashes that key together with the store format, what
ra-jit was built from and the toolchain's commit. What it was built from is a hash
`crates/ra-jit/build.rs` takes of every file of the workspace's crates and the vendored
ones, of `Cargo.toml`, `Cargo.lock`, `.cargo/config.toml` and `harness/toolchain`, of the
features, the target and whether debug assertions are on. An entry is
therefore found by any build of ra-jit from the same sources on any machine, for the same
toolchain: a CI runner that builds ra-jit for a commit that changes only the harnesses or
the docs reuses the store of the commit before (`harness/storebuilds.py`). A ra-jit built
outside this workspace (the published crate) keys by the bytes of its executable, as every
build did before. Upgrading ra-jit makes the old
entries unreachable; they are removed over time as least recently used.

A build script's run is kept under a key with the checkout and the target directory apart:
its package's directory and the target directory its `OUT_DIR` is in are named by what stands
for them in the key and in what it printed and wrote, and put back as the checkout that reads
it has them. A run that wrote a file that is no text naming either is kept for its own place
alone, since a path of another length would break that file. The key still hashes the
script's executable, which differs between target directories (what ra-jit links differs
with what the store had, and an rlib holds the `OUT_DIR` an `include!` named), so a second
checkout runs its scripts again until a script is keyed by what it was built from
([#105](https://github.com/indexable-inc/blaze/issues/105)).

`harness/keys.py` tests the keying: it changes, one at a time, each thing code can depend
on that is not the text of its file, and checks that a build with a store filled before the
change still prints what cargo's build prints.

### Proc-macro expansions

The store also keeps what derive and attribute macros expand to, so a daemon that loads a
workspace again does not run them again (`crates/ra-jit-expand`). The key is a SHA-256 of
the macro library's content, the macro's name and kind, its input with each span replaced
by its place in the call's table of spans, that table (each span's byte range within the
item it is anchored to, and which of the call's anchors and hygiene contexts it has), the
environment and the current directory the macro is given. The output is kept with its spans
written the same way, so tokens the macro made from a string at the call site come back
anchored where the next call is. An expansion is not kept when it failed, when it asked
rust-analyzer anything while it ran (a span's text, file or line), or when its output has a
span anchored where no span of its input is.

A macro that reads a file or an environment variable by itself is pure to the key and is
not: name it in `RA_JIT_PROC_MACRO_CACHE_SKIP` (a comma-separated list of macro names) to
expand it every time. `RA_JIT_PROC_MACRO_CACHE=all` keeps function-like macros too, which
are off by default because they are the ones that read outside their input (`sqlx::query!`,
`include_dir!`). `RA_JIT_PROC_MACRO_CACHE=check` expands every call anyway and says which
macro expanded otherwise than it was kept. `RA_JIT_PROC_MACRO_CACHE=off` keeps nothing. The
daemon's log has a line per load: how many expansions ran and in how long, how many came
from the store, and why the rest were not kept. rustc's own cache, `-Zcache-proc-macros`,
keeps derives only ([research/proc-macro-cache.md](research/proc-macro-cache.md)).
`harness/expansions.py` tests it.

### Two machines and one commit

A store is shared between machines only if each key in it is a function of what it is the key of,
and the bytes under it do not name the machine that made them. `harness/dist/crossmachine.py`
(unit C4 of [distributed.md](distributed.md)) builds one commit on two machines made on one
host, each with its own `HOME`, cargo home, checkout path, target directory, store and daemon, and
sets the stores against each other. `ra-jit cache digests` lists a line for each value in the
sealed packs, `KEY LENGTH SHA256`, and two machines agree when those lines are the same.
`RA_JIT_KEYS_LOG=FILE` lists the entries a build adds with their stable names, and the proc-macro
calls with the part of their key that differs (library, input, environment, current directory),
so that a difference says what it is. Environment diagnostics name variables and fingerprint
their values; raw values are not written to the log.

What names a place of the machine, and what stands for it in the store:

| Place | In a key and in a stored value | Where |
|---|---|---|
| the cargo home (`registry/src/...`, `git/checkouts/...`) | `/cargo` | `hir-ty-codegen` `moved`, `Entry::leave` and `arrived` |
| the standard library's sources | `/rustc/<commit>` | `LibrarySources` |
| the checkout, a file below it that is not a workspace member's | `/checkout/<path>` | `hir-ty-codegen/relocate.rs` |
| a crate or a file beside the checkout (`path = "../shared"`) | `/checkout/../shared/...`, if the two share two directories | `relocate.rs`, `crate_place`, `Contents::library` |
| `CARGO_MANIFEST_DIR`, `OUT_DIR` and the macro's current directory | raw paths remain in keys and diagnostic hashes | `ra-jit-expand/src/lib.rs` |

A file of a workspace member is named by its place in the workspace, as rustc names it, which is the
same on every machine. A path that is neither below the checkout nor beside it is kept as it is: no
other machine has it, so no other machine finds an entry that names it. Code with the checkout's
place in its data is not shared.
Proc-macro keys are not yet content-only. Removing a path from a key is unsafe even when the
output does not contain it: a macro can branch on the path's length or read a file below it.
The harness diagnoses this gap rather than allowing it as a pending success.

Registry source replacements are not assumed immutable. Library identities also hash loaded
source bytes and their dependency closure, so a changed vendored body cannot reuse the old
code just because its package name, version and relative path are unchanged. The
`vendor-edit` negative arm changes a body and its cargo checksum on B and compares B's fed
program with B's changed oracle. Literal suffixes in `concat!(file!(), ...)` are text: only
the relocation's leading traversal is interpreted, never dots or slashes in that suffix.
Library body lowering discovers out-of-VFS `include_bytes!` and `include_str!` inputs before
memoizing the identity. Those observed bytes are hashed too. `vendor-payload` changes only
a binary payload and its checksum; the library and a dependent caller must print B's values.

## The disk store

The store is `ra-jit/store` in the user's cache directory (`$XDG_CACHE_HOME`, else
`~/.cache`), whatever the checkout: two checkouts, or two projects with dependencies in
common, keep a function once. `RA_JIT_CACHE` names another directory, or `off` for none.
The daemon reads the variable when it starts, so under `cargo ra-jit` it has to be set in
the environment of the command that starts the daemon; run `cargo ra-jit stop` first if
one is running. `cargo ra-jit status` says how much the store holds, the most it may hold,
and where it is.

| Variable | Meaning | Default |
|---|---|---|
| `RA_JIT_CACHE` | the store's directory, or `off` | `ra-jit/store` in the user's cache directory |
| `RA_JIT_CACHE_MAX` | the most the sealed packs may take: bytes, or a number with `K`, `M` or `G`, or `off` | a tenth of the disk the store is on that is free or the store's, at least 256M and at most 32G |
| `RA_JIT_CACHE_SEAL_AT` | size at which an open pack is sealed at the end of a build; exists for the harnesses | 4M |

The default limit is worked out again at each collection, so a disk that fills up with
other things takes room back from the store.

### What is on disk

The store is one directory. `version` names the format. `<sha256>.pack` is a sealed pack,
named by the hash of its bytes and never changed afterwards. `<pid>-<nanos>.open` is the
pack one daemon is appending to, held locked. `*.tmp` is a file on its way to another name.
`lock` is held while the directory is emptied or collected.

A sealed pack is an index, then the values. The index has, for each value, its key, its
block, where it starts in the block, its length and a checksum. The values are in blocks of
about 128K, each compressed on its own with zstd (level 3), so reading a value reads and
unpacks its block. A daemon keeps the last 32M of blocks it unpacked, since a build reads
values in about the order it added them. A value larger than 1M is a block of its own,
kept as it is, and so is a block that compression would not make smaller. An open pack is
not compressed: its records are appended as they come, and it is compressed when it is
sealed. Compiled code compresses to about a fifth: the 1.6 GB store of a helix build is
about 0.3 GB.

A daemon that finds an older format, or files with no `version`, empties the directory and
starts over. One that finds a newer format leaves it alone and runs without a store.

### Integrity and crashes

A pack's hash is checked the first time a process reads it, and each value carries a
checksum that is checked every time it is read. A pack that fails either check, or has a
block that does not unpack, is removed and what was in it is compiled again. A sealed pack
appears by a rename after it is written and synced, and the open pack it was sealed from is
removed after that. An open pack left by a daemon that died is sealed up to its last whole
record by whoever finds it.

Several daemons can share one directory. Reading and adding take no lock; only collection
and a change of format do.

If the disk fills, the daemon prints `store: nothing more is kept of this build: ...`
once, keeps compiling, and tries again on the next build. If the directory cannot be used
at all it prints `store: not used: ...` and runs without a store.

### Size and collection

A collection runs when a daemon opens the store, after it seals a pack, and on
`ra-jit cache gc`. It seals open packs left behind, removes old temporaries, merges sealed
packs smaller than 4M into one when there are more than 16 of them, and then removes the
least recently used packs until the rest fit under `RA_JIT_CACHE_MAX`. A pack counts as used
when a daemon read a value from it during a build.

Eviction is by pack, not by record. One recently used record keeps its whole pack, which
holds up to 64M of values (some 13M on disk), so a limit smaller than a few packs evicts
nearly everything. The limit counts bytes on disk, compressed.

Upgrading ra-jit leaves the old build's entries in the store, unreachable, until they are
the least recently used; `ra-jit cache gc --max 0` empties the store at once.

### Commands

These act on the store `cargo ra-jit status` names.

    ra-jit cache stats                 format, packs, bytes, limit, oldest and newest use
    ra-jit cache verify                read everything; exit 1 if a pack is not what was written
    ra-jit cache gc [--max SIZE]       collect now, to RA_JIT_CACHE_MAX or to SIZE

### Limits

- The store harnesses run in CI on Linux, Windows and macOS. `storefull.py` (a full disk)
  runs on Linux only. On Windows a pack that another process has open cannot be removed,
  so eviction of a pack in use fails and the pack is kept; this is not measured.
- Power loss was not tested. The harness kills the daemon, which does not lose the page
  cache.
- The locks are advisory, and behaviour over NFS is not tested.

## The remote

A remote is a directory, `s3://bucket/prefix`, an `https://` address it is only read from,
whose files are named by their paths with `.` for `/` (a GitHub release's assets: see "The
standard library" below), or a `blaze+http://host:port` cache server (see "The server" below).
It holds sealed packs under the hash of
their bytes, a list of the keys in each pack, and one manifest per project that lists the
packs that project's builds were served from.

    packs/<sha256>.pack          a pack as the store has it (its blocks are compressed)
    packs/<sha256>.pack.zst      a pack sent compressed whole: one a quarter smaller or more
                                 that way, as one with values over 1 MB is (those are kept
                                 uncompressed in the store, to be read in place)
    keys/<sha256>                the keys of one pack
    manifests/<project>          one line per pack
    units/<sha256>, deps/<project>   cargo's builds of dependencies, see below

**Real AWS S3 has not been run.** Everything was measured against MinIO and SeaweedFS on
the same machine over loopback. The client is the AWS SDK, so the code is the same, but
virtual-hosted addressing, IAM errors and role credentials are untested.

### By hand

    ra-jit cache push REMOTE [PROJECT]
    ra-jit cache pull REMOTE [PROJECT]

`push` sends the packs the manifest lacks and adds them to it. `pull` fetches the packs the
manifest lists that are not in the local store, as many as fit under `RA_JIT_CACHE_MAX`.
`PROJECT` picks the manifest and defaults to `default`.

### From the daemon

With `RA_JIT_REMOTE` set, the daemon pulls before its first build, pulls again when a build
misses a key and the manifest has changed, and pushes what it compiled after the first
build, then every five minutes, and once more when it is told to stop. No build waits for a
push. A daemon killed with SIGKILL pushes nothing on its way out; the next push sends it.

| Variable | Meaning | Default |
|---|---|---|
| `RA_JIT_REMOTE` | the remote the daemon keeps its store in step with | not set |
| `RA_JIT_PROJECT` | which manifest | `default` |
| `RA_JIT_REMOTE_READONLY` | never write to the remote | off |
| `RA_JIT_REMOTE_LAZY` | fetch only the key lists, and a pack when a build first asks for a key in it | off |
| `RA_JIT_REMOTE_PUSH_EVERY` | how often a running daemon pushes (`90s`, `5m`, `1h`), or `off` | `5m` |
| `RA_JIT_REMOTE_JOBS` | requests in flight at once | 8 |
| `RA_JIT_REMOTE_TIMEOUT` | seconds a request may go without an answer | 30 |
| `RA_JIT_REMOTE_SIGN` | file holding the key this machine signs manifests with | not set |
| `RA_JIT_REMOTE_TRUST` | public keys this machine accepts manifests from, comma separated | not set |

`RA_JIT_REMOTE` needs the local store (`RA_JIT_CACHE` not `off`): the remote fills and
reads it.

The owned `cargo build`, `run` and `test` path uses an explicitly selected store and remote
for its final compiler processes. Set both `RA_JIT_CACHE` and `RA_JIT_REMOTE`; the isolated
executor's default remains cache-off. Relative store, directory-remote and signing-key paths
are resolved from the command's working directory before compiler children change directories.
The compiler pulls before compilation and publishes sealed packs after a successful one-shot
build. Cache limits, project, lazy/read-only mode and manifest trust/signing policy are applied
to these compiler processes after producer preparation. This forwarding does not alter the
saved BuildSpec or the source environment already recorded in producer descriptions.

An explicitly selected S3 transport receives the caller's `AWS_*` SDK configuration and
credential-provider settings. Explicit config, credential and token-file paths work without
changing the compiler's isolated `HOME`; profile or SSO lookup that depends on the caller's
home directory is not inherited. Set explicit file locations or use an environment/role
provider supported by the existing SDK. Explicit TLS certificate locations are also retained
for network transports. Saved BuildSpec replay keeps its existing boundary: the worker can
choose a local store, but its ambient remote, signing policy and credentials are not adopted.
Producer images retain their existing local cache behavior; this does not make producer
actions or compilation run on a remote worker.

### The server

`ra-jit serve` shares today's cache layout over HTTP/1.1. It is unit C3 of
[distributed.md](distributed.md).
`ra-jit cache serve` is an alias.

    ra-jit serve --cache DIR --listen 127.0.0.1:7878

Without `--listen`, the address is `127.0.0.1:7878`. There is no authentication or TLS in
this unit. Anyone who can reach the port can read, write and delete cache objects.
An explicit non-loopback address prints a warning. Deployment access remains the
operator's Tailscale ACLs. This unit adds no capabilities or sandbox policy.

A client uses the `blaze+http` place. Addresses have a host and port, without credentials,
a path prefix or a query. `blaze+https` is refused rather than downgraded to plaintext.

    ra-jit cache push blaze+http://HOST:PORT
    ra-jit cache pull blaze+http://HOST:PORT

Push, pull, lazy fetch, the keeper, dependency units and remote collection use the existing
transfer code. The server keeps `packs/`, `keys/`, `units/`, `manifests/` and `deps/`.
A directory remote can read that layout.

The server has these calls.

| Call | Behavior |
|---|---|
| `POST /v1/hello` | The current `blaze_wire::VERSION` and server limits, encoded by `blaze-wire` in deterministic CBOR. |
| `POST /v1/cache/missing` | Up to 4,096 digests, answered from the existence set. The client splits larger requests. |
| `GET`, `HEAD`, `PUT`, `DELETE /v1/blob/<name>` | Today's blob names, such as `packs/<sha256>.pack`. |
| `GET`, `HEAD`, `PUT`, `DELETE /v1/list/<name>` | Today's text lists, with content-hash ETags and conditional PUT. |
| `GET /v1/objects?prefix=<prefix>` | A legacy complete CBOR listing, or the explicitly requested paged extension below. |

A bare `/v1/blob/<sha256>` PUT stores raw content in `units/`. GET and HEAD find the
existing raw object named by that digest. A compressed-only pack is not returned by a
bare-digest GET or HEAD. Its named `.pack.zst` route returns compressed bytes without an
ETag for the uncompressed digest. Compressed packs are named by the uncompressed pack's
digest. The general encoding-header protocol remains C5 work.
Digest path names require lowercase hex to prevent case-insensitive filesystem aliases.

A PUT streams to a temporary file. Verification hashes raw content or decompresses and hashes
a compressed pack without reading the whole upload into memory. Packs must have this store's
format header. The server then syncs the file, renames it and syncs the directory.
Interrupted or invalid uploads never appear under the final name. Concurrent publication and
deletion update the disk and existence set in the same order.
An explicit reupload replaces the whole object and refreshes its write time for collection.
Temporary names start with a dot and never appear in listings. Legacy list temporaries with
the old `.<pid>-<nanos>.tmp` suffix are also excluded before filesystem metadata is read.

GET supports partial, open-ended and suffix byte ranges. Successful ranges return `206`
with `Content-Range`. Invalid or unsatisfiable ranges return `416`. HEAD returns the same
length without a body. Lists use `If-Match` or `If-None-Match: *` to avoid lost updates.
A failed condition returns code 6 with status `409`, and the client reads again.
When a resumed download gets `416` because the remote object is shorter than the saved
prefix, the fetch drops that prefix and restarts once from zero. A malformed range response
is a final refusal and removes the partial instead of disabling other transfers.
The restarted bytes must still satisfy the listed size, digest and pack-format checks
before publication. A local upload over the negotiated size limit is not retried.

The existence set is a hint. Files changed directly behind the server are not reflected in
that set until a rescan or restart. Fetches still read the file, and clients verify its digest.
Direct filesystem writers do not participate in the server's conditional-write lock.
Listings are a best-effort snapshot. An object deleted between enumeration and its metadata
read is omitted. Other filesystem errors remain errors. A real manifest that disappears
after it was listed still causes collection to fail rather than losing its references.

`hello` returns an Ed25519 public key persisted in `DIR/key`, a placeholder Main epoch of 1,
and an empty identity set. C3 has no records or identity restrictions. Records, generation
lookup, root-based collection and coordinator jobs remain separate units.
On Unix the key seed is created with `0600` permissions. Reopening an older key fixes its mode.

The `/v1` route family is not the CBOR codec version. Both peers advertise
`blaze_wire::VERSION` and require the selected version to match. This branch has the C1
codec, currently 1, and has not integrated C2's later schemas or signing domains.
Hello is itself schema-typed. A request with different fields fails as malformed before
version negotiation. An unsupported version within the same envelope gets the version error.
C2 integration must define a stable hello envelope or negotiate in headers before decoding.
It must update the route banner, docs and codec tests together. No newer or older codec is
silently ingested here.

The client validates advertised limits against the codec's supported ceiling and uses accepted
limits for missing batches, request bodies, bounded answers and small-object reads.
Zero, inconsistent or unsupported larger limits are refused.

The server gives request headers and each body frame a 30-second idle deadline.
An idle or cancelled upload removes its owned staging file without publishing it.
Response writes use the same progress-idle deadline. A peer that stops reading releases
its connection slot; a slow transfer that keeps writing can exceed the idle interval.
At most 256 connections are accepted concurrently. TCP keepalive detects dead peers.
Filesystem setup, reads, listing, deletion and publication use the blocking pool, not
async runtime workers. These are network resource leases, not compile-worker memory caps.

Every minute the server sweeps staging files older than one hour. Live staging files hold
an exclusive file lock through verification, so a sweep does not remove an active upload.
Restart sweeps old abandoned files as well. Newer crash leftovers stay hidden until eligible.
Unopenable or un-lockable janitor files are logged and left untouched. They do not stop
the service or cause locked uploads to be removed. Transient accept failures, including
descriptor exhaustion, back off and retry while existing connections can finish.
Upload publication still requires file locks; filesystems without that facility are
explicit failures, not an unsafe unlocked fallback.
The public library's `Config::resources` configures the deadlines and lease counts.

### Paginated listing extension

The C3 listing extension is versioned separately from the C1 CBOR codec.
`X-Blaze-Listing-Version: 1` requests paging on `/v1/objects`.
The query accepts `prefix`, `limit` and an optional `cursor`, each once.
The response body remains the existing `ObjectsAnswer` CBOR map.
Every page fits the advertised request-byte and batch-item limits.
`X-Blaze-Listing-Version: 1` and `X-Blaze-Listing-Total` identify a paged response.
`X-Blaze-Listing-Next` names the next page when more objects remain.

A cursor contains a random snapshot ID and its next row offset. It is not a filesystem path.
The server validates the prefix and offset against an immutable, sorted metadata snapshot.
Snapshots expire after ten minutes and their lease count is bounded by the connection limit.
An unknown or expired cursor returns `409`. An invalid cursor or query returns `400`.
When leases are occupied, a new snapshot returns `429`; it never truncates a listing.

Each snapshot captures all names once. Later publication does not insert rows between pages.
Deletion after capture does not shift offsets or skip later objects.
The client checks the declared total, strict ordering, typed names and cursor progress.
It follows every page before returning a list to GC. A real manifest deleted after capture
still fails its subsequent read, so GC does not silently lose references.
A fresh listing sees subsequent publication and deletion.
Snapshot construction is proportional to directory size and uses memory for metadata.
This is not a claim of unlimited cache scale or a root-based GC implementation.

Requests without the extension header retain the complete legacy response.
An oversized legacy response still fails with `413`, not a partial result.
The new client accepts a complete legacy small response from an older server.
Older clients can use the new server's legacy route unchanged.
No CBOR field, `blaze_wire::VERSION`, C2 BuildSpec or signing domain changed here.

`python3 -I harness/remote.py --server` exercises the server alongside available S3 services.
`--server --transfers-only` runs its transfer, damage and signing checks without compiler
round trips. Neither command measures a speedup.
`--selftest` checks given-S3, cache-server, MinIO and SeaweedFS dispatch without credentials.
The TLS fixture requires MinIO and openssl. SeaweedFS retains its normal S3 checks but does
not run the MinIO-specific TLS fixture; the harness reports that omission.
The focused server CI also runs actual remote-client tests and the dispatch selftest.
It installs Rust 1.99.0 with rust-src and does not build a release harness binary.

### The standard library

A build with nothing of rustc's (a build script, `--exe` with every crate ra-jit's) starts
from the standard library compiled by ra-jit, which takes seconds on many cores. A release
ships it: `ra-jit std REMOTE TOOLCHAIN` builds it for the binary and the toolchain at that sysroot and pushes it as
the project `std-<target>-<build>`, and the release attaches that remote's files. Before such a
build first reads the store it fetches the project once per store, from:

| Variable | Meaning | Default |
|---|---|---|
| `RA_JIT_STD_FROM` | the remote to fetch the standard library from, or `off` | the binary's own release, when it is one; otherwise none |

A build of another binary (crates.io, nix, your own) finds nothing for its `<build>` and
compiles the library itself, once per store.

### S3 address and credentials

    s3://bucket/prefix?endpoint=URL&region=NAME&profile=NAME&path-style=true|false

Whatever the address leaves out comes from where the AWS tools look: `AWS_ENDPOINT_URL`,
`AWS_REGION`, `AWS_PROFILE`, `~/.aws/config`. An unknown option is refused by name. With an
endpoint of its own, ra-jit addresses the bucket by path unless told `path-style=false`.

ra-jit does not hold credentials; the AWS SDK finds them (environment, shared config and
profiles, web identity, container and instance roles). They are not printed. The harness
searches everything ra-jit printed in a run for the secret key.

### Trust and signing

There are two questions.

Can the storage change the code? No. A pack is named by the SHA-256 of its bytes and is
hashed after the fetch and before it is put in the store. A remote, proxy or disk that
returns other bytes has them thrown away and reported, and the build compiles those
functions itself. Key lists and cargo units are checked the same way.

Can someone with write access make ra-jit use the wrong code? Without signing, yes. The
manifest is the one mutable object. Somebody who can write to the bucket can upload a
well-formed pack with wrong code under real keys and list it. Content addressing does not
catch that.

Signing does. Make a key and note the public half it prints:

    ra-jit cache keygen ~/.config/ra-jit/sign.key

A machine with `RA_JIT_REMOTE_SIGN` naming that file signs each manifest it writes
(Ed25519, over the manifest's name and body). A machine with `RA_JIT_REMOTE_TRUST` listing
public keys refuses any manifest not signed by one of them. A machine that trusts keys but
has none to sign with refuses to push.

So: with `RA_JIT_REMOTE_TRUST` set, nobody without a trusted signing key can make ra-jit emit
code other than what a local build gives, whatever they do to the bucket. Without it,
whoever can write to the bucket is trusted, as with sccache or a registry mirror. A trusted
signer is trusted completely; ra-jit does not compile again to check them. Under all of it
sits one assumption: two machines with the same ra-jit binary compile the same key to
equivalent code.

The harness checks, against both servers and the cache server of "The server" below: a pack
truncated, extended, emptied or with one byte flipped; a different valid pack under another's
name; a listed pack that is missing; and manifests that are forged, unsigned, or signed by an
unlisted key.

There is no `SECURITY.md` in the repository at the commit these docs were written against.

### Collecting a remote

    ra-jit cache gc --remote REMOTE [--unlisted-after AGE] [--older-than AGE] [--dry-run]

An age is a number and `s`, `m`, `h` or `d`. The run first takes lines out of manifests:
with `--older-than`, the lines of packs written longer ago than that, and always the lines
of packs whose every key is also in other packs of the same manifest. It then removes
objects no manifest names that were written longer ago than `--unlisted-after`, which
defaults to a day and may not be less than two hours.

It is safe against a push in progress by the clock, not by a lock: a push relies on an
unlisted object only for an hour after it was written. A single push that takes more than
an hour between sending a pack and listing it, or a machine whose clock is hours off, is
outside what it promises. Nothing runs a remote collection on its own.

### Dependencies built by cargo

    ra-jit deps push REMOTE DIR [PROJECT]
    ra-jit deps pull REMOTE DIR [PROJECT]

Run in the directory of `Cargo.lock`, where `DIR` is the directory cargo builds a profile
in, such as `target/debug`. This sends and fetches what cargo built of registry and git
dependencies (rlibs, build script output, fingerprints) so that a fresh checkout's first
cargo build finds them fresh. It is separate from the function store and is tested by
`harness/deps.py`.

### What it buys, measured

On one machine over loopback, a fresh second "machine" (another home directory, checkout
and store) building ripgrep 14.1.1 took 16.8 s to a running program with no remote, 14.3 s
with ra-jit's packs, and 9.6 s with packs and cargo's units. For cargo 0.92.0 the same three
were 152.4 s, 117.1 s and 94.4 s. In each case with a remote, ra-jit compiled 0 functions.
What is left is cargo running build scripts and proc macros, and ra-jit loading the
workspace and walking every instance to find its key. Lazy fetching was slightly slower
than fetching everything in both. Transfer times over loopback are a lower bound and say
nothing about a real network. The full tables are in [results.md](results.md).

### Limits

- Real AWS has not been run.
- Windows and macOS were not run for the remote; the harness is Linux only.
- The cache server's verification results are reported per run. No server timing is claimed.
- A cache server holds no records yet: a push still sends every pack its manifest lacks, and
  there is no batch lookup of a build's crates (C5).
- When one fetch runs out of retries the remote is taken to be down and that process's
  other transfers stop. The next run resumes them.
- A merged pack is held twice on the remote until someone runs `cache gc --remote`.
