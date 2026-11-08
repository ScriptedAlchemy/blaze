//! A proc macro: its unit is compiled for the macro host, and goes the same way as any other.

use proc_macro::TokenStream;

/// The item it is put on, and a function saying the item's name was seen.
#[proc_macro_attribute]
pub fn seen(_attr: TokenStream, item: TokenStream) -> TokenStream {
    check_folded_bodies();
    let mut out: TokenStream =
        "pub fn seen() -> &'static str { \"seen by the macro\" }".parse().unwrap();
    out.extend(item);
    out
}

// The proc macro runs these wasm bodies: enough members and IR to exercise real groups.
#[inline(never)]
fn folded<const N: u64>(mut value: u64) -> u64 {
    for _ in 0..16 {
        value = (value ^ N).wrapping_mul(3).rotate_left(5);
    }
    value
}

fn check_folded_bodies() {
    let input = std::hint::black_box(11u64);
    macro_rules! cases {
        ($($n:literal),*) => { 0u64 $(.wrapping_add(folded::<$n>(input)))* };
    }
    let actual = cases!(
        1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
        26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48,
        49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64
    );
    let mut expected = 0u64;
    for n in 1..=64 {
        let mut value = input;
        for _ in 0..16 {
            value = (value ^ n).wrapping_mul(3).rotate_left(5);
        }
        expected = expected.wrapping_add(value);
    }
    assert_eq!(actual, expected, "folded wasm bodies preserve every member's constants");
}
