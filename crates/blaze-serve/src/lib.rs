//! `blaze-serve`: blaze's HTTP/1.1 cache server for content-addressed packs and unit pack lists.
//!
//! Pure `#![no_std]` driver over `blaze-sys`, `blaze-own`, `blaze-sha256`, and `blaze-wire`.
//!
//! Routes:
//! - `GET /` — health banner (`"blaze-serve\n"`)
//! - `GET /v1/blob/packs/<sha256>.pack` — stream a pack (`200`, `ETag: "<sha256>"`)
//! - `PUT /v1/blob/packs/<sha256>.pack` — verify `DIST_PACK_MAGIC` and SHA-256, publish (`204`)
//! - `GET /v1/list/manifests/<sha256>` — read a unit's pack list (`200`)
//! - `PUT /v1/list/manifests/<sha256>` — verify packs and publish (optional `If-Match` CAS; `204` / `412`)

#![no_std]
#![deny(missing_docs)]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::driver]
#![deny(blaze::std_use, blaze::implicit_allocator, blaze::process_static)]

mod blob;
mod error;
mod server;
mod store;
mod sys;

pub use blob::{Kind, ObjectName};
pub use error::{Error, ErrorCode, ErrorMessage, ServeError};
pub use server::{Config, Resources, Server, ShutdownHandle, serve, serve_on};
pub use store::Store;
pub use sys::{SysError, process_args};

/// Writes `bytes` to standard error (`fd 2`) without pulling in `std::io`.
pub fn write_stderr(bytes: &[u8]) {
    sys::write_fd_best_effort(sys::Fd::STDERR, bytes);
}
