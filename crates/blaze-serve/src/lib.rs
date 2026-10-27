//! blaze's cache server: today's remote layout over HTTP/1.1.
//!
//! `ra-jit serve --cache DIR --listen ADDR` serves the directory a `Dir` place would be, so a
//! machine's `ra-jit cache push|pull`, the daemon's keeper and a lazy build reach it as they
//! reach a directory or a bucket, and share one cache instead of one copy each. What it adds
//! over a directory is what a shared one needs: every blob is checked against its name before
//! it becomes visible, `missing` is answered from memory, and a blob is sent from any byte on,
//! so a transfer that broke off is taken up where it stopped.
//!
//! # The wire
//!
//! Deterministic CBOR, `application/cbor`, as `crates/blaze-wire` writes and refuses it. The
//! bytes of a blob are not CBOR: they are the object's own bytes. `docs/distributed.md`
//! section 8 gives the calls; this unit implements the ones a cache needs today, and leaves
//! `auth`, the records of C5, the results of Phase 4, the roots and GC of C7, and the
//! coordinator's jobs to them:
//!
//! | Call | Request | Answer |
//! |---|---|---|
//! | `POST /v1/hello` | `HelloRequest` | `HelloAnswer` |
//! | `POST /v1/cache/missing` | `CacheMissingRequest` | `CacheMissingAnswer` |
//! | `GET`, `HEAD`, `PUT`, `DELETE /v1/blob/<name>` | bytes | bytes |
//! | `GET`, `HEAD`, `PUT`, `DELETE /v1/list/<name>` | text | text |
//! | `GET /v1/objects?prefix=<p>` | | `ObjectsAnswer` |
//!
//! `<name>` is an object of today's layout, spelled as [cache.md](../../docs/cache.md) spells
//! it and as the `Dir` place names it: `packs/<sha256>.pack`, `packs/<sha256>.pack.zst`,
//! `keys/<sha256>`, `units/<sha256>`, and `manifests/<project>` or `deps/<project>` for a list.
//! Section 8 writes a blob's path as `/v1/blob/<sha256>`; a name here carries that digest and
//! the one part of it a path does not say: which of today's four kinds of blob it is.
//! `blobs/<sha256>` is C5's table, for when records replace the names. A bare
//! `/v1/blob/<sha256>` is answered too, by the object whose name that digest is.
//!
//! # What it does not do
//!
//! No authentication (C6), no records, namespaces or epochs (C5, C6), no GC by roots (C7). It
//! listens on `127.0.0.1` unless told otherwise, so it is a machine's own cache until C6 gives
//! it a way to tell who is asking.
//!
//! # On disk
//!
//! The directory is a remote as [cache.md](../../docs/cache.md) describes it, and nothing
//! else: a directory place can be pointed at it, and `ra-jit cache gc --remote` can collect
//! it. A blob is written under a temporary name, synced, renamed and its directory synced, so
//! a partially written one never appears under its name.

#![deny(missing_docs)]

mod blob;
mod error;
mod list;
mod server;
mod store;

pub use blob::{Kind, ObjectName, Range, parse_range};
pub use error::{Error, ErrorCode};
pub use list::{ListedObject, ObjectsAnswer};
pub use server::{Body, Config, Resources, Server, listed, serve, serve_on};
pub use store::{Expect, Listed, Store, Verification};
