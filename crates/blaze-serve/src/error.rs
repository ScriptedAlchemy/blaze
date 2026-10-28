//! Section 8's one error shape: a code, a message, and the milliseconds to wait.
//!
//! Every answer that is not a success is a [`WireError`](blaze_wire::WireError) in CBOR, with
//! the HTTP status of its code's class. A blob's answer is not CBOR and carries the same body.

use blaze_wire::WireError;

pub use blaze_wire::ErrorCode;

/// One failure, as the server answers it.
#[derive(Clone, Debug)]
pub struct Error {
    /// The code of section 8, whose class is the HTTP status.
    pub code: ErrorCode,
    /// What went wrong, in one line, for the person reading a log or a message.
    pub message: String,
    /// The milliseconds a client waits before asking again, for a code that says so.
    pub retry_after_ms: Option<u64>,
}

impl Error {
    /// An error of `code`, saying `message`.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Error {
        Error { code, message: message.into(), retry_after_ms: None }
    }

    /// Code 1: the bytes are not the message they claim to be.
    pub fn malformed(message: impl Into<String>) -> Error {
        Error::new(ErrorCode::Malformed, message)
    }

    /// Code 2: the version is not one this server speaks.
    pub fn version(message: impl Into<String>) -> Error {
        Error::new(ErrorCode::Version, message)
    }

    /// Code 5: no such blob, list or path.
    pub fn not_found(message: impl Into<String>) -> Error {
        Error::new(ErrorCode::NotFound, message)
    }

    /// Code 7: past one of the limits the server answered `hello` with.
    pub fn too_large(message: impl Into<String>) -> Error {
        Error::new(ErrorCode::TooLarge, message)
    }

    /// Code 10: the server's own fault.
    pub fn internal(message: impl Into<String>) -> Error {
        Error::new(ErrorCode::Internal, message)
    }

    /// The HTTP status of the code's class.
    pub fn status(&self) -> u16 {
        self.code.status()
    }

    /// This error as section 8's shape, for the body of the answer.
    pub fn wire(&self) -> WireError {
        let mut error = WireError::new(self.code, self.message.clone());
        error.retry_after_ms = self.retry_after_ms;
        error
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
        Error::internal(error.to_string())
    }
}

impl From<hyper::http::Error> for Error {
    fn from(error: hyper::http::Error) -> Error {
        Error::internal(error.to_string())
    }
}

/// Code 10, with what the server was doing.
pub(crate) fn failed(what: &str, error: impl std::fmt::Display) -> Error {
    Error::internal(format!("{what}: {error}"))
}
