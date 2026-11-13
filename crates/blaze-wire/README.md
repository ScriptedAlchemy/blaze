# blaze-wire

`blaze-wire` defines the types of a distributed build ([distributed.md](../../docs/distributed.md)'s
"Data shapes" and "The calls"). It encodes them as deterministic CBOR and provides validation,
digests, errors, limits and a JSON rendering for people.

## The encoding

RFC 8949 section 4.2's core deterministic encoding: shortest heads, definite lengths, map keys
ascending by their encoded bytes, no floats, no tags. A struct is a CBOR map of small integers, every
key present; an enum is the two-element array `[discriminant, payload]`; a tuple is an array; a
`Vec` marked **set** in the doc is a [`Set`], sorted by the bytes of each item's encoding.

```rust
let job = JobRequest { unit, platform, namespace: Namespace::Main, source: [0; 32], deadline_ms, local };
let bytes = blaze_wire::encode(&job);
let back: JobRequest = blaze_wire::decode(&bytes, &Limits::DOCUMENTED)?;
```

One value has one encoding, so a request or a result can be hashed and kept under that hash.

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

## Digests

A digest is `sha256("blaze/" ‖ name ‖ "/v5" ‖ 0x00 ‖ canonical CBOR of the value)`, where `name` is
the type's name. The domain separation means a `WorkKey`'s and a `JobContentInput`'s bytes never
collide even when their encodings do. [`digest_input`] returns the preimage itself.

## Golden vectors

`tests/golden/` holds one value of every type of [`vectors::every`], its canonical CBOR and its
digest. `examples/write_golden.rs` writes them again, which is for a change of the schema, and a
change of the schema changes `VERSION` and the digests' `/v5`.

## Limits

[`Limits::DOCUMENTED`]: 16 MiB a request, 1 MiB an event, 4 GiB a blob. Every message is held to
them, before anything is allocated.
