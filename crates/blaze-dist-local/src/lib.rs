//! Local Unix IO carrier for the sans-IO distributed protocol. This crate runs no compiler.
//! A single nonblocking reactor owns the coordinator; clocks and effect delivery are injected.

#![no_std]
#![feature(allocator_ext, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::driver]
#![deny(missing_docs)]
#![deny(blaze::std_use, blaze::implicit_allocator, blaze::process_static)]

pub mod agent;
pub mod blobs;
pub mod client;
pub mod coordinator;
pub mod driver;
pub mod pack;
pub mod protocol;
pub mod result_gate;
pub mod server;
pub mod sys;
pub mod telemetry;

#[cfg(test)]
mod allocator_tests;

use core::fmt;

use blaze_wire::WireString;

/// A distributed execution, compiler, protocol or IO failure. None permits implicit fallback.
#[derive(Debug)]
pub enum Error {
    /// Unix or TCP IO failure.
    Io(sys::IoError),
    /// The peer sent an invalid/version-mismatched response.
    Protocol(WireString),
    /// Committed distributed data is missing or corrupt; never fall back under its key.
    Committed(WireString),
    /// The final compiler or output exporter refused the unit; never recompile its refusal.
    Refused(WireString),
    /// The caller cancelled the build; never recompile it under local fallback.
    Cancelled(WireString),
    /// The unit's execution ended with nothing committed, as its terminal says.
    Ended(blaze_wire::job_terminal_v2::TerminationV2),
}

impl From<sys::IoError> for Error {
    fn from(error: sys::IoError) -> Self {
        Self::Io(error)
    }
}

impl From<sys::Errno> for Error {
    fn from(errno: sys::Errno) -> Self {
        Self::Io(sys::IoError::from_errno(errno))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl core::error::Error for Error {}
