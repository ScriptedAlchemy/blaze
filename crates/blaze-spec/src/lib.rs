//! The `BuildSpec` of docs/distributed.md, computed from cargo's resolution (unit C2).
//!
//! A requester gives a cargo command's flags, the directory it was run in and its environment;
//! this crate resolves them the way blaze's own build does (`blaze-manifest` reads the
//! configuration and the profiles, `blaze-metadata` resolves the units) and returns the
//! [`BuildSpec`](blaze_wire::BuildSpec) with its `ExecPlatform` and keys, or a [`Refusal`] that
//! names the flag, profile setting or variable blaze cannot honour, before anything is scheduled.
//!
//! - [`flags`]: cargo's flags read strictly into a [`flags::Request`].
//! - [`platform`]: the `ExecPlatform` of this machine, with the native tools by digest.
//! - [`assemble`]: a request, the configuration and the units made into a spec.
//! - [`run`]: a spec made back into the flags and environment that do what it says.

#![deny(missing_docs)]

pub mod assemble;
pub mod flags;
pub mod platform;
mod refuse;
pub mod run;

pub use refuse::Refusal;
