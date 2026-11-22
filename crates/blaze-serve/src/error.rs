//! The one error shape: a code, a message, and the milliseconds to wait.
//!
//! Every answer that is not a success is a [`WireError`](blaze_wire::WireError) in CBOR, with
//! the HTTP status of its code's class. A blob's answer is not CBOR and carries the same body.

use blaze_wire::WireError;

use std::{net::SocketAddr, path::PathBuf};

pub use blaze_wire::ErrorCode;

/// A failure to start or continue the cache service, before an HTTP answer can be made.
#[derive(Debug)]
pub enum ServeError {
    /// Opening the named persistent store failed.
    Store {
        /// The persistent store being opened.
        directory: PathBuf,
        /// The store's refusal.
        source: Error,
    },
    /// The task opening the named store failed before returning its result.
    StoreTask {
        /// The persistent store being opened.
        directory: PathBuf,
        /// The task's failure.
        source: tokio::task::JoinError,
    },
    /// The listening address could not be bound.
    Listen {
        /// The requested listening address.
        address: SocketAddr,
        /// The operating system's bind failure.
        source: std::io::Error,
    },
    /// The listener could not report its local address.
    ListenerAddress(std::io::Error),
    /// Accepting a connection failed outside the transport's retryable conditions.
    Accept {
        /// The listener that failed.
        address: SocketAddr,
        /// The operating system's accept failure.
        source: std::io::Error,
    },
    /// The service's connection capacity was closed while it was accepting work.
    Capacity(tokio::sync::AcquireError),
    /// Installing or receiving the shutdown signal failed.
    Signal(std::io::Error),
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store { directory, source } => {
                write!(f, "opening cache store {}: {source}", directory.display())
            }
            Self::StoreTask { directory, source } => {
                write!(f, "opening cache store {}: task failed: {source}", directory.display())
            }
            Self::Listen { address, source } => write!(f, "listening on {address}: {source}"),
            Self::ListenerAddress(source) => write!(f, "reading the listener's address: {source}"),
            Self::Accept { address, source } => write!(f, "accepting on {address}: {source}"),
            Self::Capacity(source) => write!(f, "acquiring a cache connection slot: {source}"),
            Self::Signal(source) => write!(f, "waiting for the cache shutdown signal: {source}"),
        }
    }
}

impl std::error::Error for ServeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match self {
            Self::Store { source, .. } => source,
            Self::StoreTask { source, .. } => source,
            Self::Listen { source, .. }
            | Self::Accept { source, .. }
            | Self::ListenerAddress(source)
            | Self::Signal(source) => source,
            Self::Capacity(source) => source,
        })
    }
}

#[derive(Debug)]
pub(crate) enum ConnectionError {
    PeerAddress(std::io::Error),
    Keepalive { peer: SocketAddr, source: std::io::Error },
    Http { peer: SocketAddr, source: hyper::Error },
}

impl std::fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PeerAddress(source) => write!(f, "reading the cache peer's address: {source}"),
            Self::Keepalive { peer, source } => {
                write!(f, "setting cache peer {peer}'s TCP keepalive: {source}")
            }
            Self::Http { peer, source } => write!(f, "serving cache peer {peer}: {source}"),
        }
    }
}

impl std::error::Error for ConnectionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match self {
            Self::PeerAddress(source) | Self::Keepalive { source, .. } => source,
            Self::Http { source, .. } => source,
        })
    }
}

/// One failure, as the server answers it.
#[derive(Clone, Debug)]
pub struct Error {
    /// The code, whose class is the HTTP status.
    pub code: ErrorCode,
    /// What went wrong, in one line, for the person reading a log or a message.
    pub message: String,
    /// The milliseconds a client waits before asking again, for a code that says so.
    pub retry_after_ms: Option<u64>,
}

impl Error {
    /// An error of `code`, saying `message`.
    pub fn new(code: ErrorCode, message: std::fmt::Arguments<'_>) -> Error {
        let wire = WireError::new(code, message);
        Error { code, message: wire.message, retry_after_ms: None }
    }

    /// Code 1: the bytes are not the message they claim to be.
    pub fn malformed(message: std::fmt::Arguments<'_>) -> Error {
        Error::new(ErrorCode::Malformed, message)
    }

    /// Code 2: the version is not one this server speaks.
    pub fn version(message: std::fmt::Arguments<'_>) -> Error {
        Error::new(ErrorCode::Version, message)
    }

    /// Code 5: no such blob, list or path.
    pub fn not_found(message: std::fmt::Arguments<'_>) -> Error {
        Error::new(ErrorCode::NotFound, message)
    }

    /// Code 7: past one of the limits the server answered `hello` with.
    pub fn too_large(message: std::fmt::Arguments<'_>) -> Error {
        Error::new(ErrorCode::TooLarge, message)
    }

    /// Code 10: the server's own fault.
    pub fn internal(message: std::fmt::Arguments<'_>) -> Error {
        Error::new(ErrorCode::Internal, message)
    }

    /// The HTTP status of the code's class.
    pub fn status(&self) -> u16 {
        self.code.status()
    }

    /// Consume this error as the wire's shape, preserving its owned message without copying.
    pub fn into_wire(self) -> WireError {
        WireError { code: self.code as u8, message: self.message, retry_after_ms: self.retry_after_ms }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.status(), self.message)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Error {
        Error::internal(format_args!("{error}"))
    }
}

impl From<hyper::http::Error> for Error {
    fn from(error: hyper::http::Error) -> Error {
        Error::internal(format_args!("{error}"))
    }
}

/// Code 10, with what the server was doing.
pub(crate) fn failed(what: &str, error: impl std::fmt::Display) -> Error {
    Error::internal(format_args!("{what}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_answer_keeps_the_original_message_allocation() {
        let mut error = Error::new(ErrorCode::Busy, format_args!("pack {} is publishing", 17));
        error.retry_after_ms = Some(250);
        let message = error.message.as_ptr();
        let wire = error.into_wire();
        assert_eq!(wire.message, "pack 17 is publishing");
        assert_eq!(wire.message.as_ptr(), message, "serialization must move the owned message");
        assert_eq!(wire.code(), Some(ErrorCode::Busy));
        assert_eq!(wire.retry_after_ms, Some(250));
    }
}
