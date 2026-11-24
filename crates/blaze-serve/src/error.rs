//! Typed errors: every refusal has a wire [`ErrorCode`], an HTTP status and a message naming
//! what failed.

use core::fmt;
use core::net::SocketAddr;

use blaze_own::{String, format_in};
pub use blaze_wire::ErrorCode;
use blaze_wire::{WireAlloc, WireError, WireString};

use crate::sys::SysError;

/// Failure to start or run the server.
#[derive(Debug)]
pub enum ServeError {
    /// Opening or verifying the store directory failed.
    Store(Error),
    /// Binding or listening on the TCP address failed.
    Listen {
        /// The address `blaze-serve` tried to bind.
        address: SocketAddr,
        /// The underlying OS error.
        source: SysError,
    },
    /// Creating the internal shutdown pipe failed.
    Pipe {
        /// The underlying OS error.
        source: SysError,
    },
    /// The accept or poll loop failed fatally.
    Loop {
        /// The underlying OS error.
        source: SysError,
    },
}

impl fmt::Display for ServeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(err) => write!(f, "opening store failed: {err}"),
            Self::Listen { address, source } => write!(f, "listen on {address}: {source}"),
            Self::Pipe { source } => write!(f, "creating wakeup pipe: {source}"),
            Self::Loop { source } => write!(f, "server poll loop failed: {source}"),
        }
    }
}

impl core::error::Error for ServeError {}

/// Failure inside one request or store operation, mapped directly to [`WireError`].
#[derive(Debug)]
pub struct Error {
    /// Wire error code.
    pub code: ErrorCode,
    /// Human-readable message naming the object and cause.
    pub message: ErrorMessage,
    /// Optional retry delay in milliseconds (becomes `Retry-After` on HTTP).
    pub retry_after_ms: Option<u64>,
}

/// Owned or static error message so static refusals never allocate.
#[derive(Debug)]
pub enum ErrorMessage {
    /// Static message string (zero heap allocation).
    Static(&'static str),
    /// Owned heap message formatted in the server's [`WireAlloc`].
    Owned(WireString),
}

impl ErrorMessage {
    /// Message text as a `&str`.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Static(s) => s,
            Self::Owned(s) => s.as_str(),
        }
    }

    /// Moves into a [`WireString`], copying only for the `Static` variant.
    pub fn into_wire_string(self, alloc: WireAlloc) -> WireString {
        match self {
            Self::Static(s) => {
                let mut out = String::with_capacity_in(s.len(), alloc);
                out.push_str(s);
                out
            }
            Self::Owned(s) => s,
        }
    }
}

impl fmt::Display for ErrorMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl core::error::Error for Error {}

impl Error {
    /// Constructs a typed error from a static message (zero allocation).
    pub const fn new_static(code: ErrorCode, message: &'static str) -> Self {
        Self { code, message: ErrorMessage::Static(message), retry_after_ms: None }
    }

    /// Constructs a typed error from an owned [`WireString`].
    pub const fn new_owned(code: ErrorCode, message: WireString) -> Self {
        Self { code, message: ErrorMessage::Owned(message), retry_after_ms: None }
    }

    /// `400 Bad Request` (`ErrorCode::Malformed`) with a static message.
    pub const fn malformed(message: &'static str) -> Self {
        Self::new_static(ErrorCode::Malformed, message)
    }

    /// `400 Bad Request` (`ErrorCode::Malformed`) with an owned message.
    pub const fn malformed_owned(message: WireString) -> Self {
        Self::new_owned(ErrorCode::Malformed, message)
    }

    /// `404 Not Found` (`ErrorCode::NotFound`) with an owned message.
    pub const fn not_found_owned(message: WireString) -> Self {
        Self::new_owned(ErrorCode::NotFound, message)
    }

    /// `404 Not Found` (`ErrorCode::NotFound`) with a static message.
    pub const fn not_found(message: &'static str) -> Self {
        Self::new_static(ErrorCode::NotFound, message)
    }

    /// `413 Payload Too Large` (`ErrorCode::TooLarge`) with an owned message.
    pub const fn too_large_owned(message: WireString) -> Self {
        Self::new_owned(ErrorCode::TooLarge, message)
    }

    /// `422 Unprocessable Entity` (`ErrorCode::Refused`) with an owned message.
    pub const fn refused_owned(message: WireString) -> Self {
        Self::new_owned(ErrorCode::Refused, message)
    }

    /// `422 Unprocessable Entity` (`ErrorCode::Refused`) with a static message.
    pub const fn refused(message: &'static str) -> Self {
        Self::new_static(ErrorCode::Refused, message)
    }

    /// `500 Internal Server Error` (`ErrorCode::Internal`) with an owned message.
    pub const fn internal_owned(message: WireString) -> Self {
        Self::new_owned(ErrorCode::Internal, message)
    }

    /// `500 Internal Server Error` (`ErrorCode::Internal`) with a static message.
    pub const fn internal(message: &'static str) -> Self {
        Self::new_static(ErrorCode::Internal, message)
    }

    /// `429 Too Many Requests` (`ErrorCode::Busy`) with a `Retry-After` delay.
    pub const fn busy(message: &'static str, retry_after_ms: u64) -> Self {
        Self {
            code: ErrorCode::Busy,
            message: ErrorMessage::Static(message),
            retry_after_ms: Some(retry_after_ms),
        }
    }

    /// Formats a store I/O error naming `context` and `path`.
    pub fn store_io(alloc: WireAlloc, context: &str, path: &str, errno: blaze_sys::Errno) -> Self {
        Self::internal_owned(format_in!(alloc, "{context} {path}: {}", SysError(errno)))
    }

    /// The message string.
    pub fn message_str(&self) -> &str {
        self.message.as_str()
    }

    /// HTTP status code corresponding to [`Self::code`].
    pub fn status_code(&self) -> u16 {
        self.code.status()
    }

    /// Standard HTTP reason phrase for [`Self::status_code`].
    pub fn reason_phrase(&self) -> &'static str {
        reason_phrase_for(self.status_code())
    }

    /// Converts into the CBOR wire error, moving any owned message without copying.
    pub fn into_wire(self, alloc: WireAlloc) -> WireError {
        WireError {
            code: self.code.as_u8(),
            message: self.message.into_wire_string(alloc),
            retry_after_ms: self.retry_after_ms,
        }
    }
}

/// Standard HTTP/1.1 reason phrase for status codes emitted by `blaze-serve`.
pub(crate) const fn reason_phrase_for(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        409 => "Conflict",
        412 => "Precondition Failed",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    }
}
