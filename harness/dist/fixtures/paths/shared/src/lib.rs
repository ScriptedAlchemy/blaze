//! A crate that is not in the workspace that depends on it: `path = "../../shared"`. It is in
//! the same repository, so every machine that has the checkout has it at the same place
//! relative to the workspace, and nowhere else the same.

/// Where this function is written, as `file!()` and `line!()` say.
pub fn describe() -> String {
    format!("shared at {}:{}", file!(), line!())
}

pub fn literal_paths() -> (&'static str, &'static str) {
    (concat!(file!(), "/../tag"), concat!(file!(), "//./tag///"))
}

/// Panics, with a location in this crate, for any number above two.
pub fn boom(n: u32) -> u32 {
    if n > 2 {
        panic!("shared says no to {n}");
    }
    n
}

/// A generic function: its instances are compiled for the crate that asks.
pub fn double<T: core::ops::Add<Output = T> + Copy>(x: T) -> T {
    x + x
}
