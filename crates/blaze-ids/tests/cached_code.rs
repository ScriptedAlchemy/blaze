//! Exercise the actual compiler function-cache key constructor while its front-end crate
//! is parked. The production module is included, so this target cannot drift into a copy
//! of the formula it is meant to test. It provides only key-namespace coverage, not a
//! source-built compiler or an end-to-end function-cache result.

#[path = "../../hir-ty-codegen/fncache/key.rs"]
mod key;
