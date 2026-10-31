//! A crate of the workspace with a build script: an `OUT_DIR` file it includes, a file it
//! includes as bytes, an environment variable it sets, a panic and a `file!()`.

include!(concat!(env!("OUT_DIR"), "/generated.rs"));

pub static BLOB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/blob.bin"));

/// The file and line this function is written at.
pub fn where_am_i() -> (&'static str, u32) {
    (file!(), line!())
}

/// What the build script set.
pub fn built() -> &'static str {
    env!("INNER_BUILT")
}

/// A function of the table the build script wrote.
pub fn checked(n: u32) -> u32 {
    TABLE[(n as usize) % 4] + 1
}

/// Panics with an index out of bounds, at a location in this crate.
#[inline(never)]
pub fn out_of_range(i: usize) -> u32 {
    let values = vec![1, 2, 3];
    values[i]
}

/// The directory this crate is built in, which is in the program as data. Code with where the
/// checkout is in its data belongs to that checkout alone.
pub fn manifest_dir() -> &'static str {
    env!("CARGO_MANIFEST_DIR")
}
