# blaze-wire

`blaze-wire` defines the types in [distributed.md](../../docs/distributed.md)'s
"Data shapes" and section 8. It encodes them as deterministic CBOR and provides
validation, digests, signatures, errors, limits and a JSON rendering for people.
The cache, compile, test and [agent APIs](../../docs/roadmap.md#8-the-agent-api)
share this schema.

## The encoding

RFC 8949 section 4.2's core deterministic encoding: shortest heads, definite lengths, map keys
ascending by their encoded bytes, no floats, no tags. A struct is a CBOR map of small integers, every
key present; an enum is the two-element array `[discriminant, payload]`; a tuple is an array; a
`Vec` marked **set** in the doc is a [`Set`], sorted by the bytes of each item's encoding.

```rust
let request = HelloRequest {
    versions: Set::new(vec![blaze_wire::VERSION]),
    identity: StoreIdentity::from_store(3, &build_stamp, &toolchain),
    platform,
    role: 1,
};
let bytes = blaze_wire::encode(&request);
let back: HelloRequest = blaze_wire::decode(&bytes, &Limits::DOCUMENTED)?;
```

One value has one encoding, so a request, a record or a test result can be hashed and kept under
that hash.

## The validator

[`validate`] checks the bytes alone and names the first offence: a non-shortest head, an
indefinite length, unsorted keys, a duplicate key, a float, a tag, a reserved simple value (RFC
8949 section 3.3's `f8` and a byte below 32), an oversize item, trailing bytes, nesting past
`MAX_DEPTH`. [`validate_as`] reads the bytes as a type too, so a key the schema does not have,
a missing key, a text string that is not UTF-8, an unsorted or repeated set item and an unknown
enum discriminant are refused by name as well. [`decode`] is that same call: it runs a
bounded structural pass first and then typed field budgets (including the path exception below).
`MAX_DEPTH` counts every item, containers and scalars alike, so a scalar inside 31 arrays is the
deepest item a message may have.

The decoder rejects noncanonical messages rather than rewriting them.

## Digests and signatures

A digest is `sha256("blaze/" ‖ name ‖ "/v4" ‖ 0x00 ‖ canonical CBOR of the value)`, where `name` is
the type's name; a signature is Ed25519 over the same bytes with `sig/` in front of the name. The
domain separation means a `WorkKey`'s and a `JobContentInput`'s bytes never collide even when their
encodings do, and a signature can never be replayed as a digest or as another type's. [`digest_input`]
and [`signature_input`] return the preimages themselves, so a caller that hashes or signs elsewhere
gets the same bytes.

One value signs less than its whole encoding: a `CrateRecord`'s signature is over the six-field map
of its fields 0 to 5, under the same `sig/CrateRecord` domain, so a record carries a signature a
client checks without field 6, which is the signature itself ([`CrateRecord::signed_bytes`],
[`CrateRecord::sign`], [`CrateRecord::signature_is`]). The whole record, field 6 included, is what
`digest_of` hashes.

## Golden vectors

`tests/golden/` holds one value of every type, its canonical CBOR, its digest and its signature,
plus `credentials.txt` for section 8's `Blaze-Agent` request, `Blaze-Capability` header, `ops` bits
and `JobContent` bytes. The keys are derived from the type's name, so no secret is checked in. After
a schema change that is meant to change the vectors, write them again and review the diff:

```
cargo run -p blaze-wire --example write_golden
```

## JSON

[`to_json`] renders a value for people through `Accept: application/json` or `--json`.
It renders field numbers as text, byte strings as lowercase hex, sets as arrays
and enums as `[discriminant, payload]`. For example, `Namespace::Main` is `[0,null]`.
The protocol uses CBOR. JSON does not determine identity.

## Schema version 4

Version 4 retains required `BuildSpec.workspace_manifest` (field 12), a logical path
relative to the declared source root, and carries it in FrontEndKeyInput (5) and
ConfigKeyInput (6). `cwd` remains relative to that workspace's directory. These
are not absolute checkout paths or replacements for the source Git tree.
SourcePath escapes native byte components reversibly: ASCII letters/digits and
`-_.` are literal, all other bytes use uppercase `%HH`. `/` separates components;
`.` denotes the root. Colon/backslash and raw Unix bytes are not path separators.
Decoded slash/NUL, empty/dot/parent components and noncanonical escapes are rejected.
Generation and decoding represent `cwd` as a `SourcePath`. The manifest basename
must be `Cargo.toml`. Missing location fields and unknown fields are rejected
without defaults. Field 7's config values are `[0, literal_text]` or
`[1, workspace_relative_path]`.
The old untagged config string encoding is invalid in schemas 2 and 3. The serialized tag determines
whether a worker keeps the literal or joins the selected workspace path.

Config values also support `[2, native_bytes]` and script env values use
the same tags. NUL-free native bytes preserve raw Unix values without replacement.
Literal environment values use NUL-free `EnvText`. Individual rustflags and cfg
values use `InputText`. These retain CBOR text encoding but use the
`Limits.request` budget rather than the 4 KiB identifier limit.
The raw-byte variant and tagged script-env representation require schema 4;
text budget changes alone do not change encoding.
Native tags are canonical only for non-UTF-8, NUL-free values. Valid UTF-8 uses
Literal. Environment map names must be nonempty, exclude `=` and NUL, and appear
only once. Ambient `script_env` does not admit `WorkspaceRelative` origins.
Configuration values stay in `config_env` and are materialized only for local
probing and execution.
This prevents materialized checkout paths from entering portable script identities.

This crate speaks schema 4 only. Prior versions require a separately pinned codec at
the transport negotiation boundary; this implementation does not advertise it or
silently assign their records version-4 identity. Unchanged structural types may
decode older bytes; version negotiation and typed domains must gate their identity.
Every typed digest and signature domain is `/v4`, as recorded in the regenerated
golden vectors. HTTP routes retain `/v1`, and request authentication retains
`blaze/req/v1`. Those transport versions have not changed.
No prior-version typed digest identity can be reused as a version-4 typed digest.

## Limits

`Hello` answers them and every message is held to them: 16 MiB a request, 1 MiB an event, 4 GiB a
blob, 4,096 items in a lookup, 65,536 in a batch, 4 KiB a string. The bytes-only validator cannot
tell a lookup's array from a batch's, so the schema's decoder holds a lookup's collection to the
4,096 of [`Limits::lookup_items`] and every other collection to [`Limits::batch_items`]. Exactly the
limit is accepted and one item more is refused, by name.

Escaped SourcePath text has a path-specific budget of `Limits.request` (default
16 MiB), not the ordinary string budget: valid native path bytes can triple when
escaped. Typed decode/validate_as first validate canonical structure within the
message budget and then enforce each field's limits. General String fields still
have the 4 KiB limit. Bytes-only validate cannot distinguish text types and keeps
the general string bound; use validate_as for path-bearing schema messages.
No length-triggered allocation occurs before the bounded structural pass.
BuildSpec generation validates the default full wire budget before producing
its identity or writing a saved spec. Transport negotiation can set other budgets;
budget failures are transport resource limits, not language-path restrictions.

## What is not here

Later units implement the server state machine and transports using this schema.
The transports are HTTP/1.1 on `hyper` and a local Unix socket, with blobs served
through `GET` and `PUT /v1/blob/<sha256>`.
