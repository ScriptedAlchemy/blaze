//! The `BuildSpec` of docs/distributed.md, computed from cargo's resolution (unit C2).
//!
//! A requester gives a cargo command's flags, the directory it was run in and its environment;
//! this crate resolves them the way blaze's own build does (`blaze-manifest` reads the
//! configuration and the profiles, `blaze-metadata` resolves the units) and returns the
//! [`BuildSpec`](blaze_wire::BuildSpec) with its `ExecPlatform` and keys, or a [`Refusal`] that
//! names the flag, profile setting or variable blaze cannot honour, before anything is scheduled.
//!
//! - [`flags`]: cargo's flags read strictly into a [`flags::Request`], each honoured, ignored or
//!   refused, in one table.
//! - [`environment`]: the variables of the requester's environment, refused, carried to the
//!   build scripts or ignored.
//! - [`config`]: the keys of cargo's configuration, and `[env]` resolved.
//! - [`rustflags`]: the flags of rustc, by cargo's precedence, each honoured, ignored or refused.
//! - [`profile`]: a unit's profile as the spec's `ProfileSpec`, with blaze's backend settings.
//! - [`platform`]: the `ExecPlatform` of this machine, with the native tools by digest.
//! - [`assemble`]: a request, the configuration and the units made into a spec and its keys.
//! - [`run`]: a spec made back into the flags and environment that do what it says.
//!
//! Every refusal is of one entry of [`CATALOGUE`], the one table of what blaze refuses and why.

#![deny(missing_docs)]

pub mod assemble;
pub mod config;
pub mod environment;
pub mod execution;
pub mod flags;
pub mod git_path;
pub mod platform;
pub mod profile;
pub mod recipe;
mod refuse;
pub mod replay;
pub mod run;
pub mod rustflags;
pub mod snapshot;

pub use refuse::{Area, CATALOGUE, Entry, Reason, Refusal};
