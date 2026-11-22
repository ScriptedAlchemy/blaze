---
status: current
summary: The retained blaze-remote library's signed segment cache, its configuration and command interface, and the parked CLI boundary.
---
# The signed shared cache

`crates/blaze-remote` implements an object-bucket cache of the plain store's segments,
signed statements, index shards, roots and bundles. This library remains in the active
workspace. Its settings and command handlers are implemented; the compiler and CLI that
previously called them are [parked](v2/plan/parked.md#waiting-on-a-command-line-without-the-daemon).
The command spellings below describe the retained handler interface, not a claim that a
compiler built from this checkout can already run them. A bootstrap release has its own
interface and must not be assumed to implement this one.

The implementation is in [settings.rs](../crates/blaze-remote/src/settings.rs),
[cmd.rs](../crates/blaze-remote/src/cmd.rs) and
[http.rs](../crates/blaze-remote/src/http.rs). The retained dispatch is
`crates/blaze-cargo/src/cli/cache.rs`; it still obtains the settings directory through the
removed daemon layer. Library callers supply that directory explicitly as `root`.

This cache uses signed plain segments in an S3-compatible bucket. The distributed protocol's
standalone pack server is documented in [distributed.md](distributed.md). The older gateway
proposal is the dated [remote-cache.md](remote-cache.md); the V1 disk-store account is
[cache.md](cache.md).

## Settings and their source

The caller's `root` contains `remote.json`, optional `remote-credentials.json`, and the
plain store's `store.json`. The parser rejects unknown JSON fields. A setting present both
in a file and in a nonempty environment variable is an error, even when the values agree.
An empty environment value is treated as absent; a non-UTF-8 value is refused.

With neither a bucket in the file nor bucket settings in the environment, no cache is
configured and no network connection is made. Environment settings can configure a cache
even when `remote.json` is absent. Setting `"enabled": false` in that file disables contact
with a configured bucket too. `status` remains local; `pull` and `push` report that nothing
was contacted when disabled or unconfigured. Malformed configuration is still an error.

| Environment variable | File field | Meaning |
|---|---|---|
| `BLAZE_CACHE_ENDPOINT` | `remote.json`: `bucket.endpoint` | HTTPS S3-compatible endpoint for signed requests. |
| `BLAZE_CACHE_BUCKET` | `remote.json`: `bucket.name` | Bucket name used with the endpoint. |
| `BLAZE_CACHE_PUBLIC_URL` | `remote.json`: `bucket.public_url` | HTTPS address for anonymous GET and HEAD requests for public objects. |
| `BLAZE_CACHE_PUBLIC_ANCHORS` | `remote.json`: public entry in `namespaces[].anchors` | Comma-separated Ed25519 public keys, each 64 hex digits; whitespace around entries is removed. The public namespace cannot also be configured in the file. |
| `BLAZE_CACHE_TRANSPORT` | `remote.json`: `bucket.transport` | `h1` by default. The parser accepts `h3`, but the production bucket constructor refuses it because the HTTP/3 driver is not built; it never silently switches transport. |
| `BLAZE_CACHE_ACCESS_KEY_ID` | `remote-credentials.json`: `access_key_id` | Access key for signed S3 requests; requires its secret access key. |
| `BLAZE_CACHE_SECRET_ACCESS_KEY` | `remote-credentials.json`: `secret_access_key` | Secret paired with the access key ID; either both are set or neither is. |
| `BLAZE_CACHE_SIGNING_SEED` | `remote-credentials.json`: `signing_seed` | Builder's 32-byte Ed25519 seed encoded as 64 hex digits. It signs statements, roots and bundles. |
| `BLAZE_CACHE_KEY_LIST` | `remote-credentials.json`: `key_list` | Local path to the anchor-signed key-list bytes used by a push. The path is read as supplied, so a relative path is relative to the process's working directory. |
| `BLAZE_CACHE_ANCHOR_SEED` | Environment only | Separate 32-byte Ed25519 seed, 64 hex digits, used by `keys sign` and optionally `keys public`. It is not a remote-settings field or a credential sent to the bucket. |

Both addresses must have the form `https://<host>` with no path or embedded credentials;
trailing slashes are accepted. A credentials file is refused if any group or other
permission bit is set; mode `0600` is the usual setting. Diagnostics redact the secret
access key and signing seeds. Public keys may be printed.

The remaining `remote.json` fields have no environment equivalent:

| Field | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Whether a configured cache may be contacted. |
| `bucket.cas` | `true` | Use conditional replacement of the root. With `false`, the pusher writes then reads back its root and reports a lost race if another writer replaced it. |
| `namespaces` | Empty list | Namespace names and their trusted anchor public keys. Every namespace read or pushed needs anchors. |
| `root` | `main` | Name of the root object within a namespace and toolchain. |
| `sample_per_1024` | `64` | Sampling setting passed to the push verifier; values above `1024` are capped at `1024`. |

A public reader needs a public address and the public namespace's anchor keys. Its
`remote.json` has this shape; the angle-bracketed value must be replaced with the actual
trusted public key:

```json
{
  "enabled": true,
  "bucket": {"public_url": "https://cache.example.org", "transport": "h1"},
  "namespaces": [{"name": "public", "anchors": ["<64 hex digits>"]}]
}
```

Private reads and writes need the signed endpoint, bucket and access-key pair. A push also
needs a builder signing seed and signed key list. The handler names missing publication
settings before making network requests. Configuring credentials does not itself publish
anything; pushes are explicit operations.

## Store namespaces and publication

The store's permissions are independent of bucket credentials. They are defined in
`crates/blaze-cas/src/plain/settings.rs`. A store reads local, then its optional organisation,
then public. Without `trusted_writer`, every record it writes is local and cannot be pushed.
An organisation is opt-in through `org` in `store.json`; naming it permits reads, not writes.
A trusted writer can also write public and its configured organisation namespace.

The cache handlers operate on the plain segment store. Its configuration can explicitly
select `"form": "plain"`; the store's default form when no file exists is packs. `status`
prints which namespaces the store reads and writes, whether remote contact is configured,
and where settings came from. It makes no bucket connection and prints no secret value.

| Retained command | Handler behavior |
|---|---|
| `blaze cache status` | Reports local configuration and store namespace policy. |
| `blaze cache pull <toolchain> <target> <cfg> [<namespace>]` | Defaults to public; fetches the signed bundle and adopts its segments after namespace, statement and content checks. Refuses a namespace the store does not read. |
| `blaze cache push '<json>'` | Publishes segments from a permitted nonlocal namespace, with signed statements, index and root, and an optional bundle. Refuses a store whose `trusted_writer` is off. |
| `blaze cache keys public` | Prints the public keys derived from the configured builder seed and/or anchor seed; no network contact. |
| `blaze cache keys sign '<json>'` | Signs a key list with the environment's anchor seed and writes the resulting bytes to `out`; no network contact. |

The push input requires `toolchain`; `namespace` defaults to `public` and `tier` to `top`
(`fast` is the other tier). Optional `bundle` holds `target` and `cfg`. The key-list input
requires `namespace`, `serial`, `issued`, `keys` and `out`. Each key entry holds its public
`key`, `roles` (`build`, `root`, or both), `from` and `until` Unix seconds, and optional
`revoked` (default `false`). A validity interval with `from > until` is refused. See
`crates/blaze-remote/src/cmd.rs` for the exact typed inputs.

## Trust and evidence

Namespace anchors sign key lists. Those lists grant builder keys roles and validity
intervals. Builders sign segment statements, roots and bundles; the signed data binds the
namespace and toolchain as well as the content. Local namespace data has no bucket name.
A missing anchor, unknown signer, invalid signature, wrong namespace or corrupt segment is
a refusal, not permission to use unchecked bytes. The layouts and verification rules have
one implementation in `crates/blaze-remote/src/trust.rs` and the plain store.

`crates/blaze-remote/src/tests.rs` contains simulated bucket faults, concurrent pushes,
namespace separation, trust refusals, bundle round trips, no-contact configuration checks
and the trusted-writer refusal. These library tests exercise the implemented cache. They
do not establish that the parked compiler automatically reads or fills it, or that a
particular live bucket honours conditional writes. That deployment still needs its own
end-to-end evidence.
