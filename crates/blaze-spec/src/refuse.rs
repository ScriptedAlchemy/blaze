use std::fmt;

/// What blaze cannot honour of a build, named, before it is scheduled.
///
/// `what` is the flag, profile setting, variable or tool exactly as the requester wrote or has
/// it (`--target`, `profile.release.lto = "thin"`, `RUSTC_WRAPPER`); `why` says what blaze does
/// not do. A refused spec is not a failed build: the requester builds it locally.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The thing refused, as the requester wrote it.
    pub what: String,
    /// Why blaze does not honour it.
    pub why: String,
}

impl Refusal {
    /// A refusal of `what` for `why`.
    pub fn new(what: impl Into<String>, why: impl Into<String>) -> Refusal {
        Refusal { what: what.into(), why: why.into() }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.what, self.why)
    }
}

impl std::error::Error for Refusal {}
