# blaze-wire

blaze's wire schema: every type of [docs/distributed.md](../../docs/distributed.md)'s "Data shapes"
and section 8, in deterministic CBOR, with its validator, its digests and signatures, its errors and
limits, and a JSON rendering for people. One crate holds the schema for the cache, compile and test
API and the agent API ([docs/roadmap.md](../../docs/roadmap.md#8-the-agent-api)).

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

[`validate`] checks the bytes alone and names the first offence: a non-shortest head, an indefinite
length, unsorted keys, a duplicate key, a float, a tag, an oversize item, trailing bytes, nesting
past `MAX_DEPTH`. [`validate_as`] reads the bytes as a type too, so a key the schema does not have,
a missing key, a text string that is not UTF-8, an unsorted or repeated set item and an unknown
enum discriminant are refused by name as well.

Nothing canonicalizes a bad message: a client that encodes wrong finds out.

## Digests and signatures

A digest is `sha256("blaze/" ‖ name ‖ "/v1" ‖ 0x00 ‖ canonical CBOR of the value)`, where `name` is
the type's name; a signature is Ed25519 over the same bytes with `sig/` in front of the name. The
domain separation means a `WorkKey`'s and a `JobContentInput`'s bytes never collide even when their
encodings do, and a signature can never be replayed as a digest or as another type's.

## Golden vectors

`tests/golden/` holds one value of every type, its canonical CBOR, its digest and its signature,
plus `credentials.txt` for section 8's `Blaze-Agent` request, `Blaze-Capability` header, `ops` bits
and `JobContent` bytes. The keys are derived from the type's name, so no secret is checked in. After
a schema change that is meant to change the vectors, write them again and review the diff:

```
cargo run -p blaze-wire --example write_golden
```

## JSON

[`to_json`] renders a value for people (`Accept: application/json`, `--json`): field numbers as text,
byte strings as lowercase hex, sets as arrays. It is never the wire and nothing's identity is taken
from it.

## Limits

`Hello` answers them and every message is held to them: 16 MiB a request, 1 MiB an event, 4 GiB a
blob, 4,096 items in a lookup, 65,536 in a batch, 4 KiB a string.

## What is not here

The transport (HTTP/1.1 on `hyper`, a Unix socket locally, blobs by `GET`/`PUT /v1/blob/<sha256>`)
and the server's state machine are not this crate's: it is the schema, and the units after C1 build
on it.
