//! Local Unix IO carrier for the sans-IO distributed protocol. This crate runs no compiler.
//! A single nonblocking reactor owns the coordinator; clocks and effect delivery are injected.

#![deny(missing_docs)]

pub mod client;
pub mod coordinator;
pub mod protocol;
pub mod server;

use std::io;

/// A framing, codec, protocol or IO failure. None triggers an implicit local compiler fallback.
#[derive(Debug)]
pub enum Error {
    /// Unix IO failure.
    Io(io::Error),
    /// The peer sent an invalid/version-mismatched response.
    Protocol(String),
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for Error {}
