//! blaze's cache server: what the executors of a distributed build share, over HTTP/1.1.
//!
//! A unit's executors put the packs of what they compiled here before they report it, so a
//! committed result's bytes exist, and a unit that linked names its packs in a list, so a later
//! build of the same unit takes them and schedules nothing ([docs/distributed.md](../../docs/distributed.md)).
//!
//! | Call | Request | Answer |
//! |---|---|---|
//! | `GET`, `PUT /v1/blob/packs/<sha256>.pack` | bytes | bytes |
//! | `GET`, `PUT /v1/list/manifests/<sha256>` | text | text |
//!
//! A pack's name is the SHA-256 of its bytes, and a pack is checked against its name before it
//! becomes visible, so a name only ever holds what it says. A list's name is its unit's digest;
//! a list is written once, and a second writer's `PUT` answers 412. Conditional repair uses
//! one exact quoted SHA256 `If-Match`, and can replace only a matched list whose pack is
//! absent after every replacement pack verifies; a valid or corrupt old list is never erased.
//!
//! # What it does not do
//!
//! No authentication: it listens on `127.0.0.1` unless told otherwise, so a shared one belongs
//! behind what authenticates its clients. No collection: its directory grows until it is removed.
//!
//! # On disk
//!
//! A blob is written under a temporary name, synced, renamed and its directory synced, so a
//! partially written one never appears under its name.

#![deny(missing_docs)]

mod blob;
mod error;
mod server;
mod store;

pub use blob::{Kind, ObjectName};
pub use error::{Error, ErrorCode};
pub use server::{Body, Config, Resources, Server, serve, serve_on};
pub use store::Store;
